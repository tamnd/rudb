//! End to end tests, from a string of SQL to rows.
//!
//! The layers below have their own tests and this file does not repeat them. `rudb-parse` proves
//! the grammar, `rudb-bind` proves that a query binds to the plan it should, and `rudb-exec` proves
//! that a plan produces the right rows. What is only testable here is the three of them agreeing:
//! a name a query writes has to be the name the binder resolves and the name the executor reads,
//! and a type the binder decided has to be the type the operator produces.

use std::time::Duration;

use rudb_common::{Field, LogicalType, Span, Value, days_from_civil};

use crate::{Config, Database, VECTOR_SIZE, arrow};

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

#[test]
fn non_recursive_ctes_run_as_inlined_queries() {
    let db = Database::new();
    assert_eq!(
        rows(
            &db,
            "WITH a AS (SELECT 2 AS x), b(y) AS NOT MATERIALIZED (SELECT x + 1 FROM a) SELECT y FROM b",
        ),
        vec![vec![Value::Integer(3)]]
    );
    assert_eq!(
        rows(&db, "WITH t AS (SELECT 1 AS x) SELECT left_t.x + right_t.x FROM t left_t, t right_t",),
        vec![vec![Value::Integer(2)]]
    );
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

/// `count()` with nothing in it is the third spelling of `count(*)`. It counts rows, it is named
/// after the function it really is, and it works wherever the other two do.
#[test]
fn count_with_no_arguments_counts_rows_the_way_a_star_does() {
    let db = database();
    assert_eq!(rows(&db, "SELECT count() FROM t"), vec![vec![Value::BigInt(4)]]);
    assert_eq!(rows(&db, "SELECT COUNT() FROM t"), vec![vec![Value::BigInt(4)]]);
    assert_eq!(db.query("SELECT count() FROM t").unwrap().names(), ["count_star()"]);
    assert_eq!(rows(&db, "SELECT count() OVER () FROM t LIMIT 1"), vec![vec![Value::BigInt(4)]]);
    assert_eq!(db.query("SELECT count() OVER () FROM t").unwrap().names(), ["count() OVER ()"]);
}

/// A `FILTER` picks which rows one aggregate reads, which is not the same as picking which rows
/// the query has. Every answer here was read off the pinned binary.
#[test]
fn a_filter_narrows_one_aggregate_and_leaves_the_rest_of_the_query_alone() {
    let db = scripted(&[
        "CREATE TABLE f (k INTEGER, v INTEGER)",
        "INSERT INTO f VALUES (1, 1), (1, 2), (1, NULL), (2, 3), (2, 4), (2, NULL)",
    ]);
    assert_eq!(
        rows(&db, "SELECT sum(v) FILTER (WHERE v > 1) FROM f"),
        vec![vec![Value::HugeInt(9)]]
    );
    assert_eq!(
        rows(&db, "SELECT count(*) FILTER (WHERE v > 1) FROM f"),
        vec![vec![Value::BigInt(3)]]
    );
    assert_eq!(
        rows(&db, "SELECT count(v) FILTER (WHERE v IS NULL) FROM f"),
        vec![vec![Value::BigInt(0)]]
    );
    assert_eq!(
        rows(&db, "SELECT count(*) FILTER (WHERE v IS NULL) FROM f"),
        vec![vec![Value::BigInt(2)]]
    );
    // Two aggregates over the same rows, one of them filtered, which is what the feature is for.
    assert_eq!(
        rows(&db, "SELECT count(*) FILTER (WHERE v > 1), count(*) FROM f"),
        vec![vec![Value::BigInt(3), Value::BigInt(6)]]
    );
    // A group where the predicate holds for nothing still has its row, with the empty answer each
    // aggregate gives over no rows at all.
    assert_eq!(
        rows(
            &db,
            "SELECT k, count(*) FILTER (WHERE v > 2), sum(v) FILTER (WHERE v > 2) \
             FROM f GROUP BY k ORDER BY k"
        ),
        vec![
            vec![integer(1), Value::BigInt(0), Value::Null],
            vec![integer(2), Value::BigInt(2), Value::HugeInt(7)],
        ]
    );
    assert_eq!(rows(&db, "SELECT sum(v) FILTER (WHERE false) FROM f"), vec![vec![Value::Null]]);
    assert_eq!(
        rows(&db, "SELECT sum(DISTINCT v) FILTER (WHERE v > 1) FROM f"),
        vec![vec![Value::HugeInt(9)]]
    );
    // The predicate is cast to BOOLEAN the way a `WHERE` is, so an integer column is a predicate
    // on whether the integer is not zero and the null row drops out of it.
    assert_eq!(rows(&db, "SELECT sum(v) FILTER (WHERE v) FROM f"), vec![vec![Value::HugeInt(10)]]);
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

/// The hash path against the loop it replaces, on the rows that tell them apart.
///
/// An equality between two columns is answered by a lookup and anything else is answered by the
/// nested loop, so `ON l.k = r.k AND l.k >= r.k` runs the loop over a join that means exactly what
/// `ON l.k = r.k` means: the second conjunct is implied by the first whenever the first is true,
/// and when either side is null both of them are null. That gives an oracle inside one engine, and
/// it is the same oracle the hash join in #351 keeps the loop around for.
///
/// The rows are the cases the two disagree about if the lookup gets its null rule from grouping.
/// Nulls on both sides, which match nothing rather than each other. Duplicates on both sides, so a
/// key with two rows on the left and three on the right is six pairs. A key on one side only, for
/// the padding each outer kind does. A two column key, because the key is a row and not a value.
#[test]
fn an_equality_is_looked_up_and_answers_what_the_loop_answers() {
    let db = Database::new();
    db.execute("CREATE TABLE l (k INTEGER, j INTEGER, tag VARCHAR)").unwrap();
    db.execute(
        "INSERT INTO l VALUES (1, 1, 'one'), (1, 1, 'one again'), (2, 2, 'two'), \
         (NULL, 1, 'null key'), (3, NULL, 'null second'), (4, 4, 'left only')",
    )
    .unwrap();
    db.execute("CREATE TABLE r (k INTEGER, j INTEGER, tag VARCHAR)").unwrap();
    db.execute(
        "INSERT INTO r VALUES (1, 1, 'a'), (1, 1, 'b'), (1, 1, 'c'), (2, 9, 'wrong second'), \
         (NULL, 1, 'null key'), (5, 5, 'right only')",
    )
    .unwrap();
    for kind in ["INNER", "LEFT", "RIGHT", "FULL"] {
        for on in ["l.k = r.k", "l.k = r.k AND l.j = r.j"] {
            let listing = |condition: &str| {
                let sql = format!(
                    "SELECT l.tag, r.tag FROM l {kind} JOIN r ON {condition} \
                     ORDER BY l.tag NULLS FIRST, r.tag NULLS FIRST"
                );
                rows(&db, &sql)
            };
            let looked_up = listing(on);
            let looped = listing(&format!("{on} AND l.k >= r.k"));
            assert_eq!(looked_up, looped, "{kind} JOIN ON {on}");
        }
    }
}

/// `IS NOT DISTINCT FROM` looked up, against the loop, on the rows where the null rule decides.
///
/// This comparison is the same lookup as `=` with the opposite answer about a null, so two nulls are
/// one key here rather than two rows that match nothing. The oracle is `coalesce` on both sides
/// against a value no row holds, which says the same thing and is a call rather than a column, so it
/// is answered by the loop.
///
/// The third condition is the one worth the most. One join with both comparisons in it is what the
/// unnesting rules write, since the domain key is equated null safely and the condition the query
/// wrote next to it is an ordinary `=`, and a table built with one rule for every column answers
/// that one wrong in a way no single comparison test would show.
#[test]
fn a_null_safe_equality_is_looked_up_and_answers_what_the_loop_answers() {
    let db = Database::new();
    db.execute("CREATE TABLE l (k INTEGER, j INTEGER, tag VARCHAR)").unwrap();
    db.execute(
        "INSERT INTO l VALUES (1, 1, 'one'), (1, 1, 'one again'), (2, 2, 'two'), \
         (NULL, 1, 'null key'), (NULL, 2, 'null key again'), (4, 4, 'left only')",
    )
    .unwrap();
    db.execute("CREATE TABLE r (k INTEGER, j INTEGER, tag VARCHAR)").unwrap();
    db.execute(
        "INSERT INTO r VALUES (1, 1, 'a'), (1, 1, 'b'), (2, 9, 'wrong second'), \
         (NULL, 1, 'null key'), (NULL, 9, 'null key again'), (5, 5, 'right only')",
    )
    .unwrap();
    let conditions = [
        ("l.k IS NOT DISTINCT FROM r.k", "coalesce(l.k, -1) = coalesce(r.k, -1)"),
        (
            "l.k IS NOT DISTINCT FROM r.k AND l.j IS NOT DISTINCT FROM r.j",
            "coalesce(l.k, -1) = coalesce(r.k, -1) AND coalesce(l.j, -1) = coalesce(r.j, -1)",
        ),
        (
            "l.k IS NOT DISTINCT FROM r.k AND l.j = r.j",
            "coalesce(l.k, -1) = coalesce(r.k, -1) AND l.j = r.j",
        ),
    ];
    for kind in ["INNER", "LEFT", "RIGHT", "FULL", "SEMI", "ANTI"] {
        for (on, oracle) in conditions {
            // A semi join and an anti join produce the driving side's columns and nothing else, so
            // there is no `r.tag` to name or to sort on.
            let one_sided = kind == "SEMI" || kind == "ANTI";
            let listing = |condition: &str| {
                let columns = if one_sided { "l.tag" } else { "l.tag, r.tag" };
                let order =
                    if one_sided { "1 NULLS FIRST" } else { "1 NULLS FIRST, 2 NULLS FIRST" };
                let sql = format!(
                    "SELECT {columns} FROM l {kind} JOIN r ON {condition} ORDER BY {order}"
                );
                rows(&db, &sql)
            };
            assert_eq!(listing(on), listing(oracle), "{kind} JOIN ON {on}");
        }
    }
}

/// The other three kinds the streaming probe answers, against the same oracle.
///
/// `INNER` and `LEFT` are two of the five and the test above has them. The other three are `SEMI`,
/// `ANTI` and `SINGLE`, and they are the ones with the least written about them and the most to get
/// wrong: a semi join keeps a driving row once however many times it matched, an anti join is that
/// question inverted, and a single join refuses a driving row that matched twice. A null rule one row
/// out shows up here and in none of the four above.
///
/// The same trick forces the loop. The second conjunct is implied by the first whenever the first is
/// true and is null whenever the first is null, so the two spellings mean the same thing and are
/// answered two different ways.
#[test]
fn a_semi_an_anti_and_a_single_join_answer_what_the_loop_answers() {
    let db = Database::new();
    db.execute("CREATE TABLE l (k INTEGER, tag VARCHAR)").unwrap();
    db.execute(
        "INSERT INTO l VALUES (1, 'one'), (1, 'one again'), (2, 'two'), (NULL, 'null key'), \
         (4, 'left only')",
    )
    .unwrap();
    db.execute("CREATE TABLE r (k INTEGER, v INTEGER)").unwrap();
    // Two rows on key 1, so a semi join has to produce the driving row once rather than twice.
    db.execute("INSERT INTO r VALUES (1, 10), (1, 11), (2, 20), (NULL, 30), (5, 50)").unwrap();
    let shapes = [
        "SELECT l.tag FROM l SEMI JOIN r ON r.k = l.k{extra}",
        "SELECT l.tag FROM l ANTI JOIN r ON r.k = l.k{extra}",
        // A scalar subquery is a single join, and this one is over the keys that match at most once
        // because the point here is the null rule rather than the error a second match raises.
        "SELECT tag, (SELECT v FROM r WHERE r.k = l.k{extra} AND r.k <> 1) FROM l",
    ];
    for shape in shapes {
        let listing = |extra: &str| {
            rows(&db, &format!("{} ORDER BY 1 NULLS FIRST", shape.replace("{extra}", extra)))
        };
        assert_eq!(listing(""), listing(" AND r.k >= l.k"), "{shape}");
    }
}

/// `*` over a semi or an anti join is the left side, and the right side is out of scope after one.
///
/// These two kinds produce the left side's rows and nothing else, so a star that expanded to both
/// sides asked the join for columns it does not have and the query died with an internal error about
/// a column not being in the schema. That is tamnd/rudb#847. The condition is still bound against
/// both sides, which is the whole point of writing one, so what changes is only what is visible
/// after the join is built.
///
/// The `USING` case is here because that clause drops a column of the right side on its way past, so
/// the boundary between the two sides has to be the one taken before the drop rather than after it.
/// Getting that wrong takes a column of the left side out of the answer.
#[test]
fn a_star_over_a_semi_or_an_anti_join_is_the_left_side_alone() {
    let db = Database::new();
    db.execute("CREATE TABLE a (k INTEGER, v INTEGER)").unwrap();
    db.execute("INSERT INTO a VALUES (1, 10), (2, 20)").unwrap();
    db.execute("CREATE TABLE b (k INTEGER, w INTEGER)").unwrap();
    db.execute("INSERT INTO b VALUES (1, 100)").unwrap();

    let matched = vec![vec![integer(1), integer(10)]];
    assert_eq!(rows(&db, "SELECT * FROM a SEMI JOIN b ON a.k = b.k"), matched);
    assert_eq!(rows(&db, "SELECT * FROM a SEMI JOIN b USING (k)"), matched);
    assert_eq!(rows(&db, "SELECT * FROM a NATURAL SEMI JOIN b"), matched);
    assert_eq!(rows(&db, "SELECT * FROM (SELECT * FROM a SEMI JOIN b ON a.k = b.k) t"), matched);
    assert_eq!(
        rows(&db, "SELECT * FROM a ANTI JOIN b ON a.k = b.k"),
        vec![vec![integer(2), integer(20)]]
    );

    // Naming the right side after the join is a binder error rather than a query about a column
    // nothing produces, which is what the reference binary says too.
    assert!(
        failure(&db, "SELECT b.w FROM a SEMI JOIN b ON a.k = b.k").contains("\"b\""),
        "the right side should be out of scope after a semi join"
    );
}

/// One driving row matching more rows than fit in a chunk.
///
/// The streaming probe answers a driving chunk into an output chunk, and a chunk holds 1024 rows, so
/// a key with five thousand rows behind it is a row that cannot be finished by the call that started
/// it. It has to come out over five calls, each picking up where the last one stopped. Getting that
/// wrong loses the tail of a popular key, which is a wrong answer that only appears on data skewed
/// enough to have one, and skew is what real data is.
#[test]
fn a_driving_row_that_matches_more_rows_than_a_chunk_holds_produces_all_of_them() {
    let db = Database::new();
    db.execute("CREATE TABLE l (k INTEGER, tag VARCHAR)").unwrap();
    db.execute("INSERT INTO l VALUES (1, 'popular'), (2, 'rare'), (3, 'absent')").unwrap();
    db.execute("CREATE TABLE r (k INTEGER, v BIGINT)").unwrap();
    db.execute("INSERT INTO r SELECT 1, i FROM range(5000) AS series(i)").unwrap();
    db.execute("INSERT INTO r VALUES (2, -1)").unwrap();
    // Every one of the five thousand, once each, and the rare key beside them rather than lost
    // behind them.
    assert_eq!(
        rows(&db, "SELECT count(*), count(DISTINCT r.v), sum(r.v) FROM l JOIN r ON l.k = r.k"),
        vec![vec![Value::BigInt(5001), Value::BigInt(5001), Value::HugeInt(12_497_499)]]
    );
    assert_eq!(
        rows(&db, "SELECT l.tag, count(*) FROM l LEFT JOIN r ON l.k = r.k GROUP BY 1 ORDER BY 1"),
        vec![
            vec![text("absent"), Value::BigInt(1)],
            vec![text("popular"), Value::BigInt(5000)],
            vec![text("rare"), Value::BigInt(1)],
        ]
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
    // A null in the middle of a column keeps its row, and the answers after it keep theirs.
    let sql = "SELECT regexp_replace(x, 'a', 'b'), regexp_extract(x, '[a-z]+') FROM (VALUES ('a1'), \
               (NULL), ('ca'), ('aa')) t(x)";
    assert_eq!(
        rows(&db, sql),
        vec![
            vec![text("b1"), text("a")],
            vec![Value::Null, Value::Null],
            vec![text("cb"), text("ca")],
            vec![text("ba"), text("aa")],
        ]
    );
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
    let message = failure(&db, "SELECT unnest([1], recursive := true)");
    assert!(message.contains("not supported yet"), "{message}");
    assert!(message.starts_with("recursive := true is not supported yet"), "{message}");
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

/// A list literal is a call to `list_value`. Per #467.
///
/// Which is what lets a column go inside one, and what makes the rule that decides the element type
/// of `[a, b]` the same rule that decides it for `list_value(a, b)`, written once.
#[test]
fn a_list_can_be_written_over_columns_and_not_only_over_constants() {
    let db = database();
    assert_eq!(rows(&db, "SELECT [x, 2] FROM t WHERE x = 3"), vec![vec![list(&[3, 2])]]);
    // Every element promotes to one type, so a list of an integer and a double holds doubles rather
    // than being refused or holding two types.
    assert_eq!(
        rows(&db, "SELECT typeof([x, 2.5::DOUBLE]) FROM t WHERE x = 3"),
        vec![vec![text("DOUBLE[]")]]
    );
    // Nothing to promote is the list of the untyped null, which is the pin's answer for `[]` and is
    // not a guess at what somebody meant to put in it.
    assert_eq!(rows(&db, "SELECT typeof([])"), vec![vec![text("\"NULL\"[]")]]);
    // The same function under its other name.
    assert_eq!(rows(&db, "SELECT list_pack(1, 2)"), vec![vec![list(&[1, 2])]]);
}

/// A list cast is a cast of every element. Per #467.
#[test]
fn a_list_casts_by_casting_every_element_of_it() {
    let db = database();
    assert_eq!(
        rows(&db, "SELECT CAST([1, 2] AS BIGINT[])"),
        vec![vec![Value::List {
            element: LogicalType::BigInt,
            values: vec![Value::BigInt(1), Value::BigInt(2)],
        }]]
    );
    assert_eq!(
        rows(&db, "SELECT [1, 2]::VARCHAR[]"),
        vec![vec![Value::List {
            element: LogicalType::Varchar,
            values: vec![text("1"), text("2")]
        }]]
    );
    // The element type comes from the target, so an empty list arrives as the thing it was asked to
    // be rather than staying the list of untyped nulls it was written as.
    assert_eq!(rows(&db, "SELECT CAST([] AS INTEGER[])"), vec![vec![list(&[])]]);
    // A depth of two is the same rule again, because the element of a list of lists is a list.
    assert_eq!(
        rows(&db, "SELECT CAST([[1], [2]] AS VARCHAR[][])"),
        vec![vec![Value::List {
            element: LogicalType::list(LogicalType::Varchar),
            values: vec![
                Value::List { element: LogicalType::Varchar, values: vec![text("1")] },
                Value::List { element: LogicalType::Varchar, values: vec![text("2")] },
            ],
        }]]
    );
}

/// `TRY_CAST` over a list nulls the element that would not go and keeps the list. Per #467.
#[test]
fn a_try_cast_of_a_list_keeps_the_elements_that_went() {
    let db = database();
    assert_eq!(
        rows(&db, "SELECT TRY_CAST(['x', '2'] AS INTEGER[])"),
        vec![vec![Value::List {
            element: LogicalType::Integer,
            values: vec![Value::Null, integer(2)],
        }]]
    );
    // Without the TRY it is the element's own failure, reported in the element's types rather than
    // in the list's.
    assert_eq!(
        failure(&db, "SELECT CAST(['x'] AS INTEGER[])"),
        "Could not convert string 'x' to INT32"
    );
    assert_eq!(
        failure(&db, "SELECT CAST([1, 2] AS BLOB[])"),
        "Unimplemented type for cast (INTEGER -> BLOB)"
    );
    // A list going somewhere that is not a list is the whole list's failure and names the list.
    assert_eq!(
        failure(&db, "SELECT CAST([1, 2] AS INTEGER)"),
        "Unimplemented type for cast (INTEGER[] -> INTEGER)"
    );
}

/// Two lists compare the way two words are ordered in a dictionary. Per #467.
#[test]
fn a_list_compares_element_by_element_and_then_by_length() {
    let db = database();
    let yes = vec![vec![Value::Boolean(true)]];
    let no = vec![vec![Value::Boolean(false)]];
    assert_eq!(rows(&db, "SELECT [1, 2] = [1, 2]"), yes);
    assert_eq!(rows(&db, "SELECT [1, 2] = [1, 3]"), no);
    assert_eq!(rows(&db, "SELECT [1, 2] < [1, 3]"), yes);
    assert_eq!(rows(&db, "SELECT ['a'] < ['b']"), yes);
    // The element of a list of lists is a list, so the rule runs at whatever depth it was written.
    assert_eq!(rows(&db, "SELECT [[1], [2]] < [[1], [3]]"), yes);
    // Nothing disagreed before one of them ran out, so the shorter one is the smaller one. A list is
    // never equal to a longer list that starts with it.
    assert_eq!(rows(&db, "SELECT [1, 2] < [1, 2, 3]"), yes);
    assert_eq!(rows(&db, "SELECT [1, 2] = [1, 2, 3]"), no);
    assert_eq!(rows(&db, "SELECT [] < [1]"), yes);
    assert_eq!(rows(&db, "SELECT [] = []"), yes);
}

/// A null inside a list and a list that is null are two different things. Per #467.
///
/// Inside a list a null is the largest value there is and equals itself, which is the sort's rule
/// rather than the comparison's. A list that is null is a null like any other and makes the whole
/// comparison null. Every row here is the pin's.
#[test]
fn a_null_inside_a_list_is_the_largest_element_and_a_null_list_is_still_null() {
    let db = database();
    let yes = vec![vec![Value::Boolean(true)]];
    let no = vec![vec![Value::Boolean(false)]];
    assert_eq!(rows(&db, "SELECT [1, NULL] = [1, NULL]"), yes);
    assert_eq!(rows(&db, "SELECT [1, NULL] = [1, 2]"), no);
    assert_eq!(rows(&db, "SELECT [1, NULL] > [1, 2]"), yes);
    assert_eq!(rows(&db, "SELECT [NULL] < [1]"), no);
    assert_eq!(rows(&db, "SELECT [1, 2] IS DISTINCT FROM [1, NULL]"), yes);
    assert_eq!(rows(&db, "SELECT NULL::INT[] = [1]"), vec![vec![Value::Null]]);
    assert_eq!(rows(&db, "SELECT NULL::INT[] IS NOT DISTINCT FROM NULL::INT[]"), yes);
}

/// Everything that rides on the order rides on it for lists too. Per #467.
///
/// `ORDER BY`, `DISTINCT`, `IN` and `max` all reach the same comparison, so the point of this is
/// that they reach it rather than that any one of them is interesting on its own.
#[test]
fn a_list_sorts_and_groups_and_maxes_like_any_other_value() {
    let db = database();
    db.execute("CREATE TABLE ls (x INTEGER[])").unwrap();
    db.execute("INSERT INTO ls VALUES ([1, NULL]), ([1, 2]), ([NULL]), (NULL), ([])").unwrap();
    // The null list sorts last because nulls sort last, and `[NULL]` sorts after `[1, NULL]` because
    // a null element is larger than any element.
    assert_eq!(
        rows(&db, "SELECT x FROM ls ORDER BY x"),
        vec![
            vec![list(&[])],
            vec![list(&[1, 2])],
            vec![Value::List {
                element: LogicalType::Integer,
                values: vec![integer(1), Value::Null],
            }],
            vec![Value::List { element: LogicalType::Integer, values: vec![Value::Null] }],
            vec![Value::Null],
        ]
    );
    assert_eq!(
        rows(&db, "SELECT max(x) FROM ls"),
        vec![vec![Value::List { element: LogicalType::Integer, values: vec![Value::Null] }]]
    );
    assert_eq!(
        rows(&db, "SELECT count(*) FROM (SELECT DISTINCT x FROM ls)"),
        vec![vec![Value::BigInt(5)]]
    );
    assert_eq!(rows(&db, "SELECT [1, 2] IN ([1, 2], [3])"), vec![vec![Value::Boolean(true)]]);
}

/// Two lists joined end to end, by the operator and by the name. Per #467.
#[test]
fn two_lists_concatenate_with_the_operator_and_with_the_name() {
    let db = database();
    assert_eq!(rows(&db, "SELECT [1, 2] || [3]"), vec![vec![list(&[1, 2, 3])]]);
    assert_eq!(rows(&db, "SELECT [1] || [2] || [3]"), vec![vec![list(&[1, 2, 3])]]);
    assert_eq!(rows(&db, "SELECT [] || [1]"), vec![vec![list(&[1])]]);
    assert_eq!(rows(&db, "SELECT [1] || []"), vec![vec![list(&[1])]]);
    // The elements promote the way a list literal's do, so the answer is a list of what the two
    // element types meet at rather than a list of the left one's.
    assert_eq!(rows(&db, "SELECT typeof([1] || [2.5::DOUBLE])"), vec![vec![text("DOUBLE[]")]]);
    assert_eq!(rows(&db, "SELECT typeof([1, 2] || [3.5])"), vec![vec![text("DECIMAL(11,1)[]")]]);
    assert_eq!(rows(&db, "SELECT typeof([] || [])"), vec![vec![text("\"NULL\"[]")]]);
    assert_eq!(
        rows(&db, "SELECT [1] || [NULL]"),
        vec![vec![Value::List {
            element: LogicalType::Integer,
            values: vec![integer(1), Value::Null],
        }]]
    );
    // The name, and the three other names the pin answers to for it.
    assert_eq!(rows(&db, "SELECT list_concat([1], [2], [3])"), vec![vec![list(&[1, 2, 3])]]);
    assert_eq!(rows(&db, "SELECT list_cat([1], [2])"), vec![vec![list(&[1, 2])]]);
    assert_eq!(rows(&db, "SELECT array_concat([1], [2])"), vec![vec![list(&[1, 2])]]);
    assert_eq!(rows(&db, "SELECT array_cat([1], [2])"), vec![vec![list(&[1, 2])]]);
    // A list column and not only a list literal, which is the row at a time path rather than the
    // folder.
    db.execute("CREATE TABLE cs (a INTEGER[], b INTEGER[])").unwrap();
    db.execute("INSERT INTO cs VALUES ([1], [2]), ([], [3, 4])").unwrap();
    assert_eq!(
        rows(&db, "SELECT a || b FROM cs ORDER BY 1"),
        vec![vec![list(&[1, 2])], vec![list(&[3, 4])]]
    );
}

/// `length` counts a list's elements and still counts a string's characters. Per #467.
///
/// It used to cast the list to a string first and count the characters of that, so
/// `length([1, 2, 3])` was 9. Every row here is the pin's.
#[test]
fn the_length_of_a_list_is_how_many_elements_it_has() {
    let db = database();
    let count = |sql: &str| rows(&db, sql);
    assert_eq!(count("SELECT length([1, 2, 3])"), vec![vec![Value::BigInt(3)]]);
    assert_eq!(count("SELECT length([])"), vec![vec![Value::BigInt(0)]]);
    assert_eq!(count("SELECT length([1, NULL])"), vec![vec![Value::BigInt(2)]]);
    assert_eq!(count("SELECT length([[1], [2]])"), vec![vec![Value::BigInt(2)]]);
    assert_eq!(count("SELECT length(NULL::INT[])"), vec![vec![Value::Null]]);
    assert_eq!(count("SELECT len([1, 2])"), vec![vec![Value::BigInt(2)]]);
    assert_eq!(count("SELECT char_length([1, 2])"), vec![vec![Value::BigInt(2)]]);
    assert_eq!(count("SELECT length('héllo')"), vec![vec![Value::BigInt(5)]]);
    assert_eq!(count("SELECT length(NULL)"), vec![vec![Value::Null]]);
    // A list column, and not only a literal the folder answers.
    db.execute("CREATE TABLE ns (x INTEGER[])").unwrap();
    db.execute("INSERT INTO ns VALUES ([1, 2]), ([]), (NULL)").unwrap();
    assert_eq!(
        rows(&db, "SELECT length(x) FROM ns"),
        vec![vec![Value::BigInt(2)], vec![Value::BigInt(0)], vec![Value::Null]]
    );
    assert!(failure(&db, "SELECT length(123)").contains("length(col0 ANY[]) -> BIGINT"));
}

/// `array_length` is the list half of `length`, with a dimension. Per #467.
#[test]
fn array_length_counts_a_list_along_its_first_dimension_and_only_that_one() {
    let db = database();
    assert_eq!(rows(&db, "SELECT array_length([1, 2, 3])"), vec![vec![Value::BigInt(3)]]);
    assert_eq!(rows(&db, "SELECT array_length([[1, 2], [3]], 1)"), vec![vec![Value::BigInt(2)]]);
    assert_eq!(rows(&db, "SELECT array_length([1, 2, 3], NULL)"), vec![vec![Value::Null]]);
    assert_eq!(rows(&db, "SELECT array_length(NULL, 1)"), vec![vec![Value::Null]]);
    assert_eq!(
        failure(&db, "SELECT array_length([1, 2], 2)"),
        "array_length for lists with dimensions other than 1 not implemented"
    );
    // No string reading, and a dimension that is not a whole number is not rounded into one.
    for refused in ["SELECT array_length('abc')", "SELECT array_length([1, 2], 1.5)"] {
        assert!(
            failure(&db, refused).contains("array_length(col0 ANY[], col1 BIGINT) -> BIGINT"),
            "{refused}"
        );
    }
}

/// A list against something that is not a list is neither reading of `||`. Per #467.
///
/// It has to be refused rather than falling back to the string reading, because the string reading
/// would answer it. `[1, 2] || 3` would be `[1, 2]3`, which is not a wrong list so much as a list
/// printed by accident.
#[test]
fn concatenating_a_list_with_something_that_is_not_one_is_refused() {
    let db = database();
    assert_eq!(
        failure(&db, "SELECT [1, 2] || 3"),
        "Cannot concatenate types INTEGER[] and INTEGER - an explicit cast is required"
    );
    assert_eq!(
        failure(&db, "SELECT 'a' || ['b']"),
        "Cannot concatenate types VARCHAR and VARCHAR[] - an explicit cast is required"
    );
    // Two lists whose elements will not meet anywhere is a different sentence, because the two
    // arguments are both the right shape and it is what is inside them that does not agree.
    assert_eq!(
        failure(&db, "SELECT [1] || ['a']"),
        "Cannot concatenate lists of types INTEGER[] and VARCHAR[] - an explicit cast is required"
    );
    // The name splits the two the same way. The wrong shape is the candidate block and elements that
    // will not meet is the sentence above.
    assert_eq!(
        failure(&db, "SELECT list_concat([1], ['a'])"),
        "Cannot concatenate lists of types INTEGER[] and VARCHAR[] - an explicit cast is required"
    );
    assert!(
        failure(&db, "SELECT list_concat([1], 2)").contains("list_concat([ANY[]...]) -> ANY[]")
    );
    // The string reading is untouched by any of this and still takes anything.
    assert_eq!(rows(&db, "SELECT 1 || 'a'"), vec![vec![text("1a")]]);
}

/// The operator propagates a null and the name skips it. Per #467.
///
/// This is the whole of the difference between `||` over two lists and `list_concat` over the same
/// two, and it is the reason the two are not one function with an alias pointing at it. The name
/// follows `concat`'s rule over strings and the operator follows every other operator's.
#[test]
fn a_null_stops_the_operator_and_is_skipped_by_the_name() {
    let db = database();
    assert_eq!(rows(&db, "SELECT [1, 2] || NULL::INT[]"), vec![vec![Value::Null]]);
    assert_eq!(rows(&db, "SELECT [1, 2] || NULL"), vec![vec![Value::Null]]);
    assert_eq!(rows(&db, "SELECT list_concat([1], NULL::INT[])"), vec![vec![list(&[1])]]);
    assert_eq!(rows(&db, "SELECT list_concat([1], NULL)"), vec![vec![list(&[1])]]);
    assert_eq!(rows(&db, "SELECT typeof(list_concat([1], NULL))"), vec![vec![text("INTEGER[]")]]);
    assert_eq!(rows(&db, "SELECT list_concat([1], [2], NULL, [3])"), vec![vec![list(&[1, 2, 3])]]);
    // Every argument was null, which is a null answer and not an empty list. An empty list argument
    // is a different thing and does give one back.
    assert_eq!(rows(&db, "SELECT list_concat(NULL, NULL)"), vec![vec![Value::Null]]);
    assert_eq!(
        rows(&db, "SELECT list_concat([], [])"),
        vec![vec![Value::List { element: LogicalType::Null, values: Vec::new() }]]
    );
}

/// `list_append` and its five relatives are `list_concat` with the value wrapped in a list, which is
/// how the pin defines them. Per #467.
///
/// So everything about them is `list_concat`'s, down to the name in the refusal, and every row here
/// is the pin's.
#[test]
fn appending_to_a_list_is_concatenating_a_list_of_one() {
    let db = database();
    let one = |sql: &str| rows(&db, sql);
    assert_eq!(one("SELECT list_append([1, 2], 3)"), vec![vec![list(&[1, 2, 3])]]);
    assert_eq!(one("SELECT array_append([1, 2], 3)"), vec![vec![list(&[1, 2, 3])]]);
    assert_eq!(one("SELECT array_push_back([1, 2], 3)"), vec![vec![list(&[1, 2, 3])]]);
    assert_eq!(one("SELECT list_prepend(0, [1, 2])"), vec![vec![list(&[0, 1, 2])]]);
    assert_eq!(one("SELECT array_prepend(0, [1])"), vec![vec![list(&[0, 1])]]);
    // The list comes first here and the value still goes in front, which is the pin's order and
    // not the one the name suggests.
    assert_eq!(one("SELECT array_push_front([1], 0)"), vec![vec![list(&[0, 1])]]);
    assert_eq!(one("SELECT list_append([], 1)"), vec![vec![list(&[1])]]);
    assert_eq!(
        one("SELECT typeof(list_append([1, 2], 3.5::DOUBLE))"),
        vec![vec![text("DOUBLE[]")]]
    );
    assert_eq!(
        one("SELECT list_append([[1]], [2])"),
        vec![vec![Value::List {
            element: LogicalType::list(LogicalType::Integer),
            values: vec![list(&[1]), list(&[2])],
        }]]
    );
    // A null list is skipped the way `list_concat` skips it, and a null value is an element.
    assert_eq!(one("SELECT list_append(NULL::INT[], 3)"), vec![vec![list(&[3])]]);
    assert_eq!(one("SELECT list_append(NULL, 3)"), vec![vec![list(&[3])]]);
    assert_eq!(one("SELECT list_prepend(1, NULL)"), vec![vec![list(&[1])]]);
    assert_eq!(
        one("SELECT list_append([1, 2], NULL)"),
        vec![vec![Value::List {
            element: LogicalType::Integer,
            values: vec![integer(1), integer(2), Value::Null],
        }]]
    );
    assert_eq!(
        failure(&db, "SELECT list_append([1], 'x'::VARCHAR)"),
        "Cannot concatenate lists of types INTEGER[] and VARCHAR[] - an explicit cast is required"
    );
    assert_eq!(
        failure(&db, "SELECT list_append([1])"),
        "Macro list_append() does not support the supplied arguments. You might need to add \
         explicit type casts.\nCandidate macros:\n\tlist_append(l, e)"
    );
    // A list column, so the expansion runs per row and not only in the folder.
    db.execute("CREATE TABLE ap (x INTEGER[], y INTEGER)").unwrap();
    db.execute("INSERT INTO ap VALUES ([1], 2), (NULL, 3)").unwrap();
    assert_eq!(
        rows(&db, "SELECT list_append(x, y) FROM ap"),
        vec![vec![list(&[1, 2])], vec![list(&[3])]]
    );
}

/// `list_transform` and `list_filter` run a lambda over every element. Per #467.
///
/// Every answer here is the pin's, headings included, since a heading is where the lambda is
/// written back out and the parameter keeps the case it was written in.
#[test]
fn a_lambda_runs_over_every_element() {
    let db = database();
    let one = |sql: &str| rows(&db, sql);
    let bigints = |values: &[i64]| Value::List {
        element: LogicalType::BigInt,
        values: values.iter().map(|&v| Value::BigInt(v)).collect(),
    };
    assert_eq!(
        one("SELECT list_transform([1, 2, 3], lambda x: x + 1)"),
        vec![vec![list(&[2, 3, 4])]]
    );
    assert_eq!(
        db.query("SELECT list_transform([1, 2, 3], lambda x: x + 1)").unwrap().names(),
        ["list_transform(list_value(1, 2, 3), (lambda x: (x + 1)))"]
    );
    assert_eq!(
        db.query("SELECT list_transform([1, 2], lambda X: x + 1)").unwrap().names(),
        ["list_transform(list_value(1, 2), (lambda X: (x + 1)))"]
    );
    // The second parameter is the position, counting from one, and it is a `BIGINT`.
    assert_eq!(
        one("SELECT list_transform([1, 2, 3], lambda x, i: x * i)"),
        vec![vec![bigints(&[1, 4, 9])]]
    );
    assert_eq!(one("SELECT list_filter([1, 2, 3, 4], lambda x: x % 2)"), vec![vec![list(&[1, 3])]]);
    assert_eq!(one("SELECT list_filter([1, 2, NULL], lambda x: NULL)"), vec![vec![list(&[])]]);
    assert_eq!(
        one("SELECT apply([1, 2], lambda x: x::VARCHAR)"),
        vec![vec![Value::List {
            element: LogicalType::Varchar,
            values: vec![text("1"), text("2")]
        }]]
    );
    assert_eq!(
        one("SELECT typeof(list_transform([1, 2], lambda x: x::DOUBLE))"),
        vec![vec![text("DOUBLE[]")]]
    );
    assert_eq!(one("SELECT list_transform(NULL, lambda x: x)"), vec![vec![Value::Null]]);
    assert_eq!(
        one("SELECT typeof(list_transform(NULL, lambda x: x))"),
        vec![vec![text("\"NULL\"")]]
    );
    assert_eq!(one("SELECT list_transform([], lambda x: x + 1)"), vec![vec![list(&[])]]);
    // A lambda inside a lambda, over the inner list and over the outer parameter.
    assert_eq!(
        one("SELECT list_transform([[1, 2], [3]], lambda x: list_transform(x, lambda y: y * 10))"),
        vec![vec![Value::List {
            element: LogicalType::list(LogicalType::Integer),
            values: vec![list(&[10, 20]), list(&[30])],
        }]]
    );
    assert_eq!(
        one("SELECT list_transform([1, 2], lambda x: list_transform([10, 20], lambda y: x + y))"),
        vec![vec![Value::List {
            element: LogicalType::list(LogicalType::Integer),
            values: vec![list(&[11, 21]), list(&[12, 22])],
        }]]
    );
    // A list longer than a chunk goes through the body in pieces, and the position carries on
    // counting across the break.
    let long: Vec<String> = (1..=3000).map(|n| n.to_string()).collect();
    assert_eq!(
        one(&format!("SELECT list_transform([{}], lambda x, i: i)[2999:3000]", long.join(", "))),
        vec![vec![bigints(&[2999, 3000])]]
    );
    assert_eq!(
        one(&format!("SELECT list_filter([{}], lambda x: x > 2998)", long.join(", "))),
        vec![vec![list(&[2999, 3000])]]
    );
}

/// A lambda's body sees the row it is in, and an aggregate in it is over the rows. Per #467.
#[test]
fn a_lambda_reads_the_columns_of_its_row() {
    let db = database();
    db.execute("CREATE TABLE lt (l INTEGER[], k INTEGER)").unwrap();
    db.execute("INSERT INTO lt VALUES ([1, 2], 10), (NULL, 20), ([], 30), ([3], 40)").unwrap();
    assert_eq!(
        rows(&db, "SELECT list_transform(l, lambda x: x + k) FROM lt"),
        vec![vec![list(&[11, 12])], vec![Value::Null], vec![list(&[])], vec![list(&[43])]]
    );
    assert_eq!(
        rows(&db, "SELECT list_filter(l, lambda x, i: i > 1) FROM lt"),
        vec![vec![list(&[2])], vec![Value::Null], vec![list(&[])], vec![list(&[])]]
    );
    assert_eq!(
        rows(&db, "SELECT list_transform([1, 2], lambda x: x + sum(k)) FROM lt"),
        vec![vec![Value::List {
            element: LogicalType::HugeInt,
            values: vec![Value::HugeInt(101), Value::HugeInt(102)],
        }]]
    );
}

/// What the pin refuses about a lambda, in its words. Per #467.
#[test]
fn a_lambda_is_refused_the_way_the_pin_refuses_it() {
    let db = database();
    assert_eq!(
        failure(&db, "SELECT list_transform([1, 0], lambda x: 10 // (x - 1))"),
        "Division by zero in expression (10 // (x - 1)). Use TRY(...) to return NULL for this \
         expression, or SET null_on_division_by_zero=true to return NULL for all divisions by zero."
    );
    assert_eq!(
        failure(&db, "SELECT list_transform([1], lambda x, y, z: x)"),
        "This lambda function only supports up to two lambda parameters!"
    );
    assert_eq!(
        failure(&db, "SELECT list_transform([1], lambda x, x: x)"),
        "table \"0_macro_parameters(x, x)\" has duplicate column name \"x\""
    );
    assert_eq!(
        failure(&db, "SELECT list_transform(1, lambda x: x)"),
        "Invalid LIST argument during lambda function binding!"
    );
    assert_eq!(
        failure(&db, "SELECT list_transform([1])"),
        "No function matches the given name and argument types 'list_transform(INTEGER[])'. You \
         might need to add explicit type casts.\n\tCandidate functions:\n\tlist_transform(col0 \
         ANY[], col1 LAMBDA) -> ANY[]\n"
    );
    assert_eq!(
        failure(&db, "SELECT list_transform([1], x -> x)"),
        "Deprecated lambda arrow (->) detected. Please transition to the new lambda syntax, i.e.., \
         lambda x, i: x + i, before DuckDB's next release.\nUse SET \
         lambda_syntax='ENABLE_SINGLE_ARROW' to revert to the deprecated behavior.\nFor more \
         information, see https://duckdb.org/docs/current/sql/functions/lambda.html."
    );
    assert_eq!(
        failure(&db, "SELECT abs(lambda x: x)"),
        "This scalar function does not support lambdas!"
    );
    assert_eq!(
        failure(&db, "SELECT list_transform([1], lambda x: (SELECT 1))"),
        "subqueries in lambda expressions are not supported"
    );
}

/// `list_reduce` folds a list left to right, and the accumulator's type is the pin's. Per #467.
///
/// The types are the part worth pinning down. The pin binds the body a second time with the
/// accumulator widened to what the first binding made, so a decimal sum gains a digit per binding,
/// and every type below is one it printed.
#[test]
fn a_reduction_folds_a_list_into_one_value() {
    let db = database();
    let one = |sql: &str| rows(&db, sql);
    assert_eq!(one("SELECT list_reduce([1, 2, 3], lambda x, y: x + y)"), vec![vec![integer(6)]]);
    assert_eq!(
        db.query("SELECT list_reduce([1, 2, 3], lambda x, y: x + y)").unwrap().names(),
        ["list_reduce(list_value(1, 2, 3), (lambda x, y: (x + y)))"]
    );
    assert_eq!(
        one("SELECT list_reduce([1, 2, 3], lambda x, y, i: x + y * i)"),
        vec![vec![Value::BigInt(14)]]
    );
    assert_eq!(
        one("SELECT list_reduce([1, 2, 3], lambda x, y: x + y, 100)"),
        vec![vec![integer(106)]]
    );
    assert_eq!(
        one("SELECT list_reduce([1, 2, 3], lambda x, y: x || y::VARCHAR, '')"),
        vec![vec![text("123")]]
    );
    assert_eq!(one("SELECT list_reduce(['a', 'b'], lambda x, y: x || y)"), vec![vec![text("ab")]]);
    assert_eq!(one("SELECT list_reduce([5], lambda x, y: x + y)"), vec![vec![integer(5)]]);
    assert_eq!(one("SELECT list_reduce([]::INT[], lambda x, y: x + y, 7)"), vec![vec![integer(7)]]);
    assert_eq!(
        one("SELECT list_reduce([1, NULL, 3], lambda x, y: x + y)"),
        vec![vec![Value::Null]]
    );
    assert_eq!(one("SELECT list_reduce(NULL, lambda x, y: x + y)"), vec![vec![Value::Null]]);
    assert_eq!(
        one("SELECT typeof(list_reduce(NULL, lambda x, y: x + y))"),
        vec![vec![text("\"NULL\"")]]
    );
    assert_eq!(
        one("SELECT list_reduce([1, 2], lambda x, y: x + y, NULL)"),
        vec![vec![Value::Null]]
    );
    assert_eq!(
        one("SELECT typeof(list_reduce([1, 2], lambda x, y: x + y, NULL))"),
        vec![vec![text("INTEGER")]]
    );
    // The position starts at 2 without an initial value, since the first element is the start.
    assert_eq!(
        one("SELECT list_reduce([1, 2, 3], lambda x, y, i: i)"),
        vec![vec![Value::BigInt(3)]]
    );
    assert_eq!(
        one("SELECT list_reduce([1, 2, 3], lambda x, y, i: i, 0)"),
        vec![vec![Value::BigInt(3)]]
    );
    // A comparison is carried as the element's type, because a boolean meets a number there.
    assert_eq!(one("SELECT list_reduce([1, 2, 3], lambda x, y: x > y)"), vec![vec![integer(0)]]);
    assert_eq!(
        one("SELECT typeof(list_reduce([1, 2], lambda x, y: x > y))"),
        vec![vec![text("INTEGER")]]
    );
    for (sql, ty, answer) in [
        ("list_reduce([1.5, 2, 3], lambda x, y: x + y)", "DECIMAL(13,1)", "6.5"),
        ("list_reduce([1, 2, 3], lambda x, y: x + y, 1.5)", "DECIMAL(12,1)", "7.5"),
        ("list_reduce([1000, 2000, 3000], lambda x, y: x + y, 1.5)", "DECIMAL(12,1)", "6001.5"),
        // The pin prints 225.00 here, one scale digit lost per step, which is tamnd/duckdb#13.
        ("list_reduce([1, 2, 3], lambda x, y: x * 1.5)", "DECIMAL(14,2)", "2.25"),
        ("list_reduce([1, 2, 3], lambda x, y: x + y + 0.5)", "DECIMAL(14,1)", "7.0"),
    ] {
        assert_eq!(one(&format!("SELECT typeof({sql})")), vec![vec![text(ty)]], "{sql}");
        assert_eq!(one(&format!("SELECT ({sql})::VARCHAR")), vec![vec![text(answer)]], "{sql}");
    }
    assert_eq!(
        one("SELECT list_reduce([[1], [2, 3]], lambda x, y: list_concat(x, y))"),
        vec![vec![list(&[1, 2, 3])]]
    );
    assert_eq!(
        one("SELECT array_reduce([1, 2], lambda x, y: x * y), reduce([1, 2], lambda x, y: x * y)"),
        vec![vec![integer(2), integer(2)]]
    );
    // Longer than a chunk, which a reduction runs a position at a time and not in batches.
    let long: Vec<String> = (1..=3000).map(|n| n.to_string()).collect();
    assert_eq!(
        one(&format!("SELECT list_reduce([{}], lambda x, y: x + y)", long.join(", "))),
        vec![vec![integer(4_501_500)]]
    );
}

/// A reduction per row, with lists of different lengths and an initial value from a column.
#[test]
fn a_reduction_runs_per_row() {
    let db = database();
    db.execute("CREATE TABLE lr (l INTEGER[], k INTEGER)").unwrap();
    db.execute("INSERT INTO lr VALUES ([1, 2], 10), (NULL, 20), ([], 30), ([3], 40)").unwrap();
    assert_eq!(
        rows(&db, "SELECT list_reduce(l, lambda x, y: x + y, k) FROM lr"),
        vec![vec![integer(13)], vec![Value::Null], vec![integer(30)], vec![integer(43)]]
    );
    assert_eq!(
        db.query("SELECT list_reduce(l, lambda x, y: x + y, k) FROM lr").unwrap().names(),
        ["list_reduce(l, (lambda x, y: (x + y)), k)"]
    );
    assert_eq!(
        rows(&db, "SELECT list_reduce(l, lambda x, y: x + y, NULL::INT) FROM lr"),
        vec![vec![Value::Null]; 4]
    );
    assert_eq!(
        rows(&db, "SELECT list_reduce(l, lambda x, y: x + y + k) FROM lr WHERE k <> 30"),
        vec![vec![integer(13)], vec![Value::Null], vec![integer(3)]]
    );
    // The empty list is refused when its row is reached, not when the query is bound.
    let error = db.query("SELECT list_reduce(l, lambda x, y: x + y + k) FROM lr").unwrap_err();
    assert_eq!(error.code(), rudb_common::ErrorCode::ParameterNotAllowed);
    assert_eq!(error.message(), "Cannot perform list_reduce on an empty input list");
}

/// What the pin refuses about a reduction, in its words. Per #467.
#[test]
fn a_reduction_is_refused_the_way_the_pin_refuses_it() {
    let db = database();
    assert_eq!(
        failure(&db, "SELECT list_reduce([], lambda x, y: x + y)"),
        "Cannot perform list_reduce on an empty input list"
    );
    assert_eq!(
        failure(&db, "SELECT list_reduce([1, 2], lambda x: x)"),
        "list_reduce expects a function with 2 or 3 arguments"
    );
    assert_eq!(
        failure(&db, "SELECT list_reduce([1, 2], lambda x, y, z, w: x)"),
        "This lambda function only supports up to three lambda parameters!"
    );
    assert_eq!(
        failure(&db, "SELECT list_reduce([1, 2])"),
        "No function matches the given name and argument types 'list_reduce(INTEGER[])'. You \
         might need to add explicit type casts.\n\tCandidate functions:\n\tlist_reduce(col0 \
         ANY[], col1 LAMBDA) -> ANY\n\tlist_reduce(col0 ANY[], col1 LAMBDA, col2 ANY) -> ANY\n"
    );
    assert_eq!(
        failure(&db, "SELECT list_reduce([1, 2], lambda x, y: [x, y])"),
        "No common super type between list element type INTEGER and lambda return type INTEGER[]"
    );
    assert_eq!(
        failure(&db, "SELECT list_reduce([1, 2], lambda x, y: [x], [0])"),
        "No common super type between initial value type INTEGER[] and lambda return type \
         INTEGER[][]"
    );
    assert_eq!(
        failure(&db, "SELECT list_reduce([1, 0], lambda x, y: x // y)"),
        "Division by zero in expression (x // y). Use TRY(...) to return NULL for this \
         expression, or SET null_on_division_by_zero=true to return NULL for all divisions by zero."
    );
    assert_eq!(
        failure(&db, "SELECT list_reduce([100, 100, 100]::TINYINT[], lambda x, y: x + y)"),
        "Overflow in addition of INT8 (100 + 100)!"
    );
}

/// `invoke` runs a lambda once per row over the arguments after it. Per #467.
#[test]
fn an_invoked_lambda_runs_over_its_arguments() {
    let db = database();
    let one = |sql: &str| rows(&db, sql);
    assert_eq!(one("SELECT invoke(lambda x: x * x, 2)"), vec![vec![integer(4)]]);
    assert_eq!(
        db.query("SELECT invoke(lambda x: x * x, 2)").unwrap().names(),
        ["invoke((lambda x: (x * x)), 2)"]
    );
    assert_eq!(one("SELECT invoke(lambda x, y: x * y + y, 3, 4)"), vec![vec![integer(16)]]);
    assert_eq!(
        one("SELECT invoke(lambda x, y, z, w, v: x + v, 1, 2, 3, 4, 5)"),
        vec![vec![integer(6)]]
    );
    assert_eq!(
        one("SELECT invoke(lambda a: invoke(lambda b, c: a + b + c, 4, 5), 3)"),
        vec![vec![integer(12)]]
    );
    assert_eq!(
        one("SELECT invoke(lambda x: invoke(lambda y: y + 5, x + 3), 2)"),
        vec![vec![integer(10)]]
    );
    assert_eq!(
        one("SELECT invoke(lambda name: invoke(lambda age: name || age, 30), 'Alice, ')"),
        vec![vec![text("Alice, 30")]]
    );
    assert_eq!(
        one("SELECT invoke(lambda x: list_transform([10, 20], lambda y, i: x + y + i), 100)"),
        vec![vec![Value::List {
            element: LogicalType::BigInt,
            values: vec![Value::BigInt(111), Value::BigInt(122)]
        }]]
    );
    assert_eq!(one("SELECT invoke(lambda x: x IS NULL, NULL)"), vec![vec![Value::Boolean(true)]]);
    assert_eq!(one("SELECT typeof(invoke(lambda x: x, NULL))"), vec![vec![text("\"NULL\"")]]);
    assert_eq!(
        one("SELECT typeof(invoke(lambda x: x, [1, 2]::INT[2]))"),
        vec![vec![text("INTEGER[2]")]]
    );
    db.execute("CREATE TABLE ti AS SELECT * FROM (VALUES (1, 10), (2, NULL), (NULL, 30)) t(a, b)")
        .unwrap();
    assert_eq!(
        one("SELECT invoke(lambda x, y: coalesce(x, 0) + coalesce(y, 0) + a, a, b) FROM ti"),
        vec![vec![integer(12)], vec![integer(4)], vec![Value::Null]]
    );
    assert_eq!(
        one(
            "SELECT invoke(lambda x: x + sum(a), 1)::INTEGER, typeof(invoke(lambda x: x + sum(a), 1)) FROM ti"
        ),
        vec![vec![integer(4), text("HUGEINT")]]
    );
    assert_eq!(
        one("SELECT i FROM range(0, 10) r(i) WHERE invoke(lambda x: x, i % 2 = 0) ORDER BY i"),
        [0, 2, 4, 6, 8].map(|i| vec![Value::BigInt(i)])
    );
    let long: Vec<Vec<Value>> = (0..3000).map(|i| vec![Value::BigInt(i * 2)]).collect();
    assert_eq!(one("SELECT invoke(lambda x: x * 2, i) FROM range(3000) r(i)"), long);
}

/// What the pin refuses about `invoke`, in its words. Per #467.
#[test]
fn an_invoked_lambda_is_refused_the_way_the_pin_refuses_it() {
    let db = database();
    let candidates = "You might need to add explicit type casts.\n\tCandidate functions:\n\tinvoke(col0 \
                      LAMBDA, col1 ANY, [ANY...]) -> ANY\n";
    assert_eq!(
        failure(&db, "SELECT invoke()"),
        format!("No function matches the given name and argument types 'invoke()'. {candidates}")
    );
    assert_eq!(
        failure(&db, "SELECT invoke(NULL)"),
        format!(
            "No function matches the given name and argument types 'invoke(\"NULL\")'. {candidates}"
        )
    );
    assert_eq!(
        failure(&db, "SELECT invoke(NULL, 1)"),
        "Invalid lambda expression passed to 'invoke' function."
    );
    assert_eq!(
        failure(&db, "SELECT invoke(1, lambda x: x)"),
        "This scalar function requires a lambda expression!"
    );
    assert_eq!(
        failure(&db, "SELECT invoke(lambda x: x + x, 2, 4, 6)"),
        "The number of lambda parameters does not match the number of arguments passed to the \
         'invoke' function, expected 1, got 3."
    );
    assert_eq!(
        failure(&db, "SELECT invoke(lambda x, y, z: x, 1)"),
        "The number of lambda parameters does not match the number of arguments passed to the \
         'invoke' function, expected at least 2, got 1."
    );
    assert_eq!(
        failure(&db, "SELECT invoke(lambda x: x + 1)"),
        "The number of lambda parameters does not match the number of arguments passed to the \
         'invoke' function, expected at least 1, got 0."
    );
    assert_eq!(
        failure(&db, "SELECT invoke(lambda x, x: x, 1, 2)"),
        "table \"0_macro_parameters(x, x)\" has duplicate column name \"x\""
    );
    // The pin ends this with " when casting from source column x", which is #1433 and not this.
    assert!(
        failure(&db, "SELECT invoke(lambda x: x::INTEGER, 'abc')")
            .starts_with("Could not convert string 'abc' to INT32")
    );
    assert_eq!(
        failure(&db, "SELECT invoke(lambda x: x * 2, 9223372036854775807::BIGINT)"),
        "Overflow in multiplication of INT64 (9223372036854775807 * 2)!"
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
    assert_eq!(rows(&db, "SELECT count(*) FROM duckdb_types()"), vec![vec![Value::BigInt(94)]]);
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
        vec![vec![Value::BigInt(16)]]
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
fn uncorrelated_scalar_subqueries_are_single_joins() {
    let db = database();
    assert_eq!(rows(&db, "SELECT (SELECT 42)"), vec![vec![Value::Integer(42)]]);
    assert_eq!(rows(&db, "SELECT 1 + (SELECT 2)"), vec![vec![Value::Integer(3)]]);
    assert_eq!(
        rows(&db, "SELECT x, (SELECT 7) FROM (VALUES (1), (2)) t(x) ORDER BY x"),
        vec![
            vec![Value::Integer(1), Value::Integer(7)],
            vec![Value::Integer(2), Value::Integer(7)]
        ]
    );
    assert_eq!(rows(&db, "SELECT (SELECT 1 WHERE false)"), vec![vec![Value::Null]]);
    let several = db
        .query("SELECT (SELECT x FROM (VALUES (1), (2)) t(x))")
        .expect_err("a scalar query has one row at most");
    assert_eq!(several.code().duckdb_name(), "Invalid Input Error");
    assert_eq!(
        several.message(),
        "More than one row returned by a subquery used as an expression - scalar subqueries can only return a single row.\n\nUse \"SET scalar_subquery_error_on_multiple_rows=false\" to revert to previous behavior of returning a random row."
    );
    // DuckDB raises it when there is no row to give the answer to as well, because it runs the
    // subquery first either way.
    let unused = db
        .query("SELECT count(*) FROM (VALUES (1)) t(x) WHERE x > 5 AND x > (SELECT y FROM (VALUES (1), (2)) u(y))")
        .expect_err("the subquery still has two rows");
    assert_eq!(unused.code().duckdb_name(), "Invalid Input Error");
    // A column wider than a vector, so the gathered row is put beside more than one chunk.
    assert_eq!(
        rows(
            &db,
            "SELECT count(*), sum(m) FROM (SELECT (SELECT max(range) FROM range(10)) AS m FROM range(5000))"
        ),
        vec![vec![Value::BigInt(5000), Value::HugeInt(45000)]]
    );
    let plan = db.plan("SELECT (SELECT 42)").expect("the scalar query plans");
    assert!(plan.contains("Join SINGLE"), "{plan}");
}

#[test]
fn scalar_subquery_multiple_row_behavior_is_a_bound_semantic() {
    let db = database();
    assert_eq!(
        rows(&db, "SELECT current_setting('scalar_subquery_error_on_multiple_rows')"),
        vec![vec![Value::Boolean(true)]]
    );
    db.execute("SET scalar_subquery_error_on_multiple_rows = false").expect("choose one row");
    assert_eq!(
        rows(&db, "SELECT (SELECT x FROM (VALUES (1), (2)) t(x))"),
        vec![vec![Value::Integer(1)]]
    );
    let plan = db
        .plan("SELECT (SELECT x FROM (VALUES (1), (2)) t(x))")
        .expect("the relaxed scalar query plans");
    assert!(plan.contains("Limit 1 offset 0"), "{plan}");
    db.execute("RESET scalar_subquery_error_on_multiple_rows").expect("restore strict mode");
    assert!(db.query("SELECT (SELECT x FROM (VALUES (1), (2)) t(x))").is_err());
    assert_eq!(
        rows(
            &db,
            "SELECT description, input_type, scope FROM duckdb_settings() WHERE name = 'scalar_subquery_error_on_multiple_rows'"
        ),
        vec![vec![
            text(
                "Throw an error when a scalar subquery returns more than one row. When disabled, an arbitrary row is returned instead."
            ),
            text("BOOLEAN"),
            text("GLOBAL"),
        ]]
    );
}

#[test]
fn a_correlated_scalar_filter_unnests_to_one_single_join() {
    let db = database();
    let sql = "SELECT k, (SELECT value FROM (VALUES (1, 10), (2, 20)) i(k, value) WHERE i.k = o.k) FROM (VALUES (1), (2), (3)) o(k) ORDER BY k";
    assert_eq!(
        rows(&db, sql),
        vec![
            vec![Value::Integer(1), Value::Integer(10)],
            vec![Value::Integer(2), Value::Integer(20)],
            vec![Value::Integer(3), Value::Null],
        ]
    );
    let plan = db.plan(sql).expect("the correlated scalar query plans");
    assert!(plan.contains("Join SINGLE"), "{plan}");
    assert!(!plan.contains("DependentJoin"), "{plan}");
    assert_eq!(
        rows(
            &db,
            "SELECT (SELECT value FROM (VALUES (1, 10), (1, 20)) i(k, value) WHERE i.k = o.k AND value > 10) FROM (VALUES (1), (2)) o(k) ORDER BY k"
        ),
        vec![vec![Value::Integer(20)], vec![Value::Null]]
    );
    db.execute("SET disabled_optimizers = 'unnest_rewriter'")
        .expect("the upstream optimizer name is accepted");
    assert_eq!(rows(&db, sql)[2], vec![Value::Integer(3), Value::Null]);
}

#[test]
fn a_correlated_scalar_projection_replays_over_distinct_outer_values() {
    let db = database();
    let sql =
        "SELECT k, (SELECT o.k + 1) FROM (VALUES (1), (1), (2), (NULL)) o(k) ORDER BY k NULLS LAST";
    assert_eq!(
        rows(&db, sql),
        vec![
            vec![Value::Integer(1), Value::Integer(2)],
            vec![Value::Integer(1), Value::Integer(2)],
            vec![Value::Integer(2), Value::Integer(3)],
            vec![Value::Null, Value::Null],
        ]
    );
    let plan = db.plan(sql).expect("the correlated scalar projection plans");
    assert!(plan.contains("Join SINGLE"), "{plan}");
    assert!(plan.contains("IS NOT DISTINCT FROM"), "{plan}");
    assert!(plan.contains("__correlated_1"), "{plan}");
    assert!(!plan.contains("DependentJoin"), "{plan}");
    assert_eq!(
        rows(
            &db,
            "SELECT k, (SELECT o.k + x FROM (VALUES (10)) i(x)) FROM (VALUES (1), (2)) o(k) ORDER BY k"
        ),
        vec![
            vec![Value::Integer(1), Value::Integer(11)],
            vec![Value::Integer(2), Value::Integer(12)],
        ]
    );
    db.execute("CREATE TABLE empty_scalar_source(x INTEGER)")
        .expect("the empty scalar source is created");
    assert_eq!(
        rows(
            &db,
            "SELECT k, (SELECT o.k + x FROM empty_scalar_source) FROM (VALUES (1), (2)) o(k) ORDER BY k"
        ),
        vec![vec![Value::Integer(1), Value::Null], vec![Value::Integer(2), Value::Null],]
    );
    assert!(
        db.query("SELECT (SELECT o.k + x FROM (VALUES (10), (20)) i(x)) FROM (VALUES (1)) o(k)")
            .is_err()
    );
}

#[test]
fn correlated_scalar_aggregates_group_by_hidden_correlation_keys() {
    let db = database();
    let sql = "SELECT k, (SELECT sum(value) FROM (VALUES (1, 10), (1, 20), (2, 5), (NULL, 40)) i(k, value) WHERE i.k = o.k AND value > 5) FROM (VALUES (1), (2), (3), (NULL)) o(k) ORDER BY k NULLS LAST";
    assert_eq!(
        rows(&db, sql),
        vec![
            vec![Value::Integer(1), Value::HugeInt(30)],
            vec![Value::Integer(2), Value::Null],
            vec![Value::Integer(3), Value::Null],
            vec![Value::Null, Value::Null],
        ]
    );
    let plan = db.plan(sql).expect("the correlated scalar aggregate plans");
    assert!(plan.contains("Join SINGLE"), "{plan}");
    assert!(plan.contains("groups=[#1.0::INTEGER]"), "{plan}");
    assert!(!plan.contains("DependentJoin"), "{plan}");
}

#[test]
fn correlated_scalar_counts_keep_zero_for_missing_groups() {
    let db = database();
    let sql = "SELECT k, (SELECT count(*) FROM (VALUES (1, 10), (1, NULL), (2, 5), (NULL, 40)) i(k, value) WHERE i.k = o.k AND value > 5) FROM (VALUES (1), (2), (3), (NULL)) o(k) ORDER BY k NULLS LAST";
    assert_eq!(
        rows(&db, sql),
        vec![
            vec![Value::Integer(1), Value::BigInt(1)],
            vec![Value::Integer(2), Value::BigInt(0)],
            vec![Value::Integer(3), Value::BigInt(0)],
            vec![Value::Null, Value::BigInt(0)],
        ]
    );
    assert_eq!(
        rows(
            &db,
            "SELECT k, (SELECT count(1) FROM (VALUES (1), (1), (2)) i(k) WHERE i.k = o.k) FROM (VALUES (1), (3)) o(k) ORDER BY k"
        ),
        vec![vec![Value::Integer(1), Value::BigInt(2)], vec![Value::Integer(3), Value::BigInt(0)],]
    );
    let plan = db.plan(sql).expect("the correlated scalar count plans");
    assert!(plan.contains("Join LEFT"), "{plan}");
    assert!(plan.contains("count(#1.0::INTEGER FILTER"), "{plan}");
    assert!(plan.contains("IS NOT DISTINCT FROM #0.0"), "{plan}");
    assert!(!plan.contains("DependentJoin"), "{plan}");
}

#[test]
fn correlated_scalar_aggregates_use_an_outer_domain_for_arbitrary_predicates() {
    let db = database();
    assert_eq!(
        rows(
            &db,
            "SELECT k, (SELECT count(*) FROM (VALUES (1), (2), (3)) i(x) WHERE i.x < o.k) FROM (VALUES (1), (2), (4)) o(k) ORDER BY k"
        ),
        vec![
            vec![Value::Integer(1), Value::BigInt(0)],
            vec![Value::Integer(2), Value::BigInt(1)],
            vec![Value::Integer(4), Value::BigInt(3)],
        ]
    );
    assert_eq!(
        rows(
            &db,
            "SELECT k, (SELECT sum(value) FROM (VALUES (1, 10), (2, 20), (3, 30)) i(x, value) WHERE i.x < o.k) FROM (VALUES (1), (2), (4)) o(k) ORDER BY k"
        ),
        vec![
            vec![Value::Integer(1), Value::Null],
            vec![Value::Integer(2), Value::HugeInt(10)],
            vec![Value::Integer(4), Value::HugeInt(60)],
        ]
    );
    assert_eq!(
        rows(
            &db,
            "SELECT k, (SELECT count(*) FROM (VALUES (10), (20)) i(x) WHERE o.k > 1) FROM (VALUES (1), (2)) o(k) ORDER BY k"
        ),
        vec![vec![Value::Integer(1), Value::BigInt(0)], vec![Value::Integer(2), Value::BigInt(2)],]
    );
    let sql = "SELECT (SELECT count(*) FROM (VALUES (NULL), (1)) i(x) WHERE i.x IS DISTINCT FROM o.k) FROM (VALUES (NULL)) o(k)";
    assert_eq!(rows(&db, sql), vec![vec![Value::BigInt(1)]]);
    let plan = db.plan(sql).expect("the arbitrary correlated aggregate plans");
    assert!(plan.contains("Join LEFT"), "{plan}");
    assert!(plan.contains("__inner_"), "{plan}");
    assert!(!plan.contains("DependentJoin"), "{plan}");
}

#[test]
fn correlated_aggregate_arguments_replay_over_the_outer_domain() {
    let db = database();
    let sql = "SELECT k, (SELECT count(o.k) FROM (VALUES (10), (20)) i(x)) FROM (VALUES (1), (NULL)) o(k) ORDER BY k NULLS LAST";
    assert_eq!(
        rows(&db, sql),
        vec![vec![Value::Integer(1), Value::BigInt(2)], vec![Value::Null, Value::BigInt(0)],]
    );
    assert_eq!(
        rows(
            &db,
            "SELECT k, (SELECT sum(o.k) FROM (VALUES (10), (20)) i(x)) FROM (VALUES (1), (NULL)) o(k) ORDER BY k NULLS LAST"
        ),
        vec![vec![Value::Integer(1), Value::HugeInt(2)], vec![Value::Null, Value::Null],]
    );
    db.execute("CREATE TABLE empty_aggregate_source(x INTEGER)")
        .expect("the empty aggregate source is created");
    assert_eq!(
        rows(&db, "SELECT (SELECT count(o.k) FROM empty_aggregate_source) FROM (VALUES (1)) o(k)"),
        vec![vec![Value::BigInt(0)]]
    );
    let plan = db.plan(sql).expect("the correlated aggregate argument plans");
    assert!(plan.contains("Join LEFT"), "{plan}");
    assert!(plan.contains("IS NOT DISTINCT FROM"), "{plan}");
    assert!(!plan.contains("DependentJoin"), "{plan}");
}

#[test]
fn uncorrelated_exists_is_a_single_joined_marker() {
    let db = database();
    let answer = db.query("SELECT EXISTS (SELECT 1)").expect("EXISTS answers");
    assert_eq!(answer.column_name(0), "EXISTS(SELECT 1)");
    assert_eq!(answer.rows().collect::<Vec<_>>(), vec![vec![Value::Boolean(true)]]);
    let negated = db.query("SELECT NOT EXISTS (SELECT 1 WHERE false)").expect("NOT EXISTS answers");
    assert_eq!(negated.column_name(0), "(NOT EXISTS(SELECT 1 WHERE false))");
    assert_eq!(negated.rows().collect::<Vec<_>>(), vec![vec![Value::Boolean(true)]]);
    assert_eq!(
        rows(&db, "SELECT EXISTS (SELECT 1 WHERE false)"),
        vec![vec![Value::Boolean(false)]]
    );
    assert_eq!(
        rows(&db, "SELECT EXISTS (SELECT NULL FROM range(3))"),
        vec![vec![Value::Boolean(true)]]
    );
    assert_eq!(
        rows(&db, "SELECT x FROM (VALUES (1), (2)) t(x) WHERE EXISTS (SELECT 1) ORDER BY x"),
        vec![vec![Value::Integer(1)], vec![Value::Integer(2)]]
    );
    assert!(
        rows(&db, "SELECT x FROM (VALUES (1), (2)) t(x) WHERE EXISTS (SELECT 1 WHERE false)")
            .is_empty()
    );
    let plan = db.plan("SELECT EXISTS (SELECT * FROM range(1000))").expect("EXISTS plans");
    assert!(plan.contains("Join SINGLE"), "{plan}");
    assert!(plan.contains("Limit 1 offset 0"), "{plan}");
}

#[test]
fn correlated_exists_ends_up_a_semi_join_against_the_relation() {
    let db = database();
    let exists = "SELECT k FROM (VALUES (1), (2), (3), (NULL)) o(k) WHERE EXISTS (SELECT 1 FROM (VALUES (1), (1), (3), (NULL)) i(k) WHERE i.k = o.k) ORDER BY k";
    assert_eq!(rows(&db, exists), vec![vec![Value::Integer(1)], vec![Value::Integer(3)]]);
    assert_eq!(
        rows(
            &db,
            "SELECT k FROM (VALUES (1), (2), (3)) o(k) WHERE NOT EXISTS (SELECT 1 FROM (VALUES (1, 5), (2, 20)) i(k, value) WHERE i.k = o.k AND value > 10) ORDER BY k"
        ),
        vec![vec![Value::Integer(1)], vec![Value::Integer(3)]]
    );
    // The keys go through a grouping and a single join on the way here, because that is what the
    // correlated key binds to, and `delim` collapses both of them once the plan is far enough
    // along to see that the grouping is only there to stop the join matching twice.
    let plan = db.plan(exists).expect("the correlated existence query plans");
    assert!(plan.contains("Join SEMI"), "{plan}");
    assert!(!plan.contains("Join SINGLE"), "{plan}");
    assert!(!plan.contains("Aggregate"), "{plan}");
    assert!(!plan.contains("DependentJoin"), "{plan}");
    assert!(!plan.contains("Limit 1"), "{plan}");
}

#[test]
fn correlated_exists_with_an_inequality_ends_up_a_semi_join() {
    let db = database();
    let sql = "SELECT k FROM (VALUES (1), (2), (4)) o(k) WHERE EXISTS (SELECT 1 FROM (VALUES (1), (3)) i(x) WHERE i.x < o.k) ORDER BY k";
    assert_eq!(rows(&db, sql), vec![vec![Value::Integer(2)], vec![Value::Integer(4)]]);
    assert_eq!(
        rows(
            &db,
            "SELECT k FROM (VALUES (1), (2), (4)) o(k) WHERE NOT EXISTS (SELECT 1 FROM (VALUES (1), (3)) i(x) WHERE i.x < o.k AND i.x > 1) ORDER BY k"
        ),
        vec![vec![Value::Integer(1)], vec![Value::Integer(2)]]
    );
    assert_eq!(
        rows(
            &db,
            "SELECT EXISTS (SELECT 1 FROM (VALUES (1)) i(x) WHERE i.x IS DISTINCT FROM o.k) FROM (VALUES (NULL)) o(k)"
        ),
        vec![vec![Value::Boolean(true)]]
    );
    let plan = db.plan(sql).expect("the correlated inequality existence query plans");
    // An inequality still decorrelates through a domain of the outer keys, and the deliminator
    // then takes the domain back out, because the outer side of the join back is the relation the
    // domain was standing in for. What is left is the two sides joined on the inequality itself.
    assert!(plan.contains("Join SEMI on=[(#1.0::INTEGER < #0.0::INTEGER)::BOOLEAN]"), "{plan}");
    assert!(!plan.contains("Join SINGLE"), "{plan}");
    assert!(!plan.contains("Aggregate"), "{plan}");
    assert!(!plan.contains("DependentJoin"), "{plan}");
}

#[test]
fn uncorrelated_in_subqueries_are_mark_joins() {
    let db = database();
    let query = |subject: &str, values: &str| {
        format!("SELECT {subject} IN (SELECT x FROM (VALUES {values}) t(x))")
    };
    assert_eq!(rows(&db, &query("1", "(1), (2)")), vec![vec![Value::Boolean(true)]]);
    assert_eq!(rows(&db, &query("3", "(1), (2)")), vec![vec![Value::Boolean(false)]]);
    assert_eq!(rows(&db, &query("3", "(1), (NULL)")), vec![vec![Value::Null]]);
    assert_eq!(rows(&db, &query("NULL", "(1), (2)")), vec![vec![Value::Null]]);
    assert_eq!(
        rows(&db, "SELECT NULL IN (SELECT x FROM (VALUES (1)) t(x) WHERE false)"),
        vec![vec![Value::Boolean(false)]]
    );
    assert_eq!(
        rows(&db, "SELECT 3 NOT IN (SELECT x FROM (VALUES (1), (NULL)) t(x))"),
        vec![vec![Value::Null]]
    );
    let answer = db
        .query("SELECT 1 IN (SELECT x FROM (VALUES (1), (2)) t(x))")
        .expect("the membership query answers");
    assert_eq!(
        answer.column_name(0),
        "(1 = ANY(SELECT x FROM (SELECT * FROM (VALUES (1), (2)) AS valueslist) AS t(x)))"
    );
    let plan = db.plan(&query("1", "(1), (2)")).expect("the membership query plans");
    assert!(plan.contains("Join MARK"), "{plan}");
}

#[test]
fn correlated_membership_filters_unnest_to_mark_joins() {
    let db = database();
    let sql = "SELECT k, k IN (SELECT x FROM (VALUES (1), (2), (NULL)) i(x) WHERE i.x = o.k) FROM (VALUES (1), (3), (NULL)) o(k) ORDER BY k NULLS LAST";
    assert_eq!(
        rows(&db, sql),
        vec![
            vec![Value::Integer(1), Value::Boolean(true)],
            vec![Value::Integer(3), Value::Boolean(false)],
            vec![Value::Null, Value::Boolean(false)],
        ]
    );
    assert_eq!(
        rows(
            &db,
            "SELECT k FROM (VALUES (1), (2), (3)) o(k) WHERE k NOT IN (SELECT x FROM (VALUES (1), (3)) i(x) WHERE i.x = o.k) ORDER BY k"
        ),
        vec![vec![Value::Integer(2)]]
    );
    assert_eq!(
        rows(
            &db,
            "SELECT k, k = ALL (SELECT x FROM (VALUES (1), (2)) i(x) WHERE i.x = o.k) FROM (VALUES (1), (3)) o(k) ORDER BY k"
        ),
        vec![
            vec![Value::Integer(1), Value::Boolean(true)],
            vec![Value::Integer(3), Value::Boolean(true)],
        ]
    );
    let plan = db.plan(sql).expect("the correlated membership query plans");
    assert!(plan.contains("Join MARK"), "{plan}");
    assert!(plan.contains("IS NOT DISTINCT FROM TRUE"), "{plan}");
    assert!(!plan.contains("DependentJoin"), "{plan}");
}

#[test]
fn correlated_mark_joins_carry_hidden_filter_columns_after_the_marker() {
    let db = database();
    let sql = "SELECT g, 15 IN (SELECT value FROM (VALUES (1, 10), (1, NULL), (2, 20)) i(g, value) WHERE i.g = o.g) FROM (VALUES (1), (2), (3)) o(g) ORDER BY g";
    assert_eq!(
        rows(&db, sql),
        vec![
            vec![Value::Integer(1), Value::Null],
            vec![Value::Integer(2), Value::Boolean(false)],
            vec![Value::Integer(3), Value::Boolean(false)],
        ]
    );
    let plan = db.plan(sql).expect("the hidden correlation key plans");
    assert!(plan.contains("Join MARK"), "{plan}");
    assert!(plan.contains("__correlated_2"), "{plan}");
    assert!(!plan.contains("DependentJoin"), "{plan}");
}

#[test]
fn uncorrelated_any_and_all_subqueries_are_mark_joins() {
    let db = database();
    assert_eq!(
        rows(&db, "SELECT 2 = ANY (SELECT x FROM (VALUES (1), (2)) t(x))"),
        vec![vec![Value::Boolean(true)]]
    );
    assert_eq!(
        rows(&db, "SELECT 2 <> ANY (SELECT x FROM (VALUES (2), (3)) t(x))"),
        vec![vec![Value::Boolean(true)]]
    );
    assert_eq!(
        rows(&db, "SELECT 2 > ALL (SELECT x FROM (VALUES (0), (1)) t(x))"),
        vec![vec![Value::Boolean(true)]]
    );
    assert_eq!(
        rows(&db, "SELECT 2 = ALL (SELECT x FROM (VALUES (2), (NULL)) t(x))"),
        vec![vec![Value::Null]]
    );
    assert_eq!(
        rows(&db, "SELECT NULL = ANY (SELECT x FROM (VALUES (1)) t(x) WHERE false)"),
        vec![vec![Value::Boolean(false)]]
    );
    assert_eq!(
        rows(&db, "SELECT NULL = ALL (SELECT x FROM (VALUES (1)) t(x) WHERE false)"),
        vec![vec![Value::Boolean(true)]]
    );
    let answer = db
        .query("SELECT 2 > ALL (SELECT x FROM (VALUES (0), (1)) t(x))")
        .expect("the universal comparison answers");
    assert_eq!(
        answer.column_name(0),
        "(NOT (2 <= ANY(SELECT x FROM (SELECT * FROM (VALUES (0), (1)) AS valueslist) AS t(x))))"
    );
    let plan = db
        .plan("SELECT 2 = ANY (SELECT x FROM (VALUES (1), (2)) t(x))")
        .expect("the quantified comparison plans");
    assert!(plan.contains("Join MARK"), "{plan}");
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
    // The pin's hundred and ninety two rows for a hundred and eighty five settings, because seven
    // of them have a second spelling and the pin gives each spelling a row of its own.
    assert_eq!(rows(&db, "SELECT count(*) FROM duckdb_settings()"), vec![vec![Value::BigInt(192)]]);
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
    // Every row has a value except the three the pin itself leaves unset, including the hundred and
    // sixty nine rudb does not read, because a client reading this table to find out what an engine
    // is set to should not find a hole where the pin has a value.
    assert_eq!(
        rows(&db, "SELECT name FROM duckdb_settings() WHERE value IS NULL ORDER BY name"),
        vec![
            vec![text("enable_profiling")],
            vec![text("operator_memory_limit")],
            vec![text("parquet_prefetch_column_gap")],
        ]
    );
}

/// A setting rudb takes and does not read still answers `SET`, `RESET`, `current_setting()` and the
/// settings table, because a script that turns a knob in its preamble wanted to keep going and not
/// to be told the engine has never heard the name.
#[test]
fn a_setting_the_engine_does_not_read_still_answers_every_way_of_asking() {
    let db = database();
    let text = |value: &str| Value::Varchar(value.to_string());
    let value = "SELECT value FROM duckdb_settings() WHERE name = 'enable_http_metadata_cache'";
    assert_eq!(rows(&db, value), vec![vec![text("false")]]);
    db.execute("SET enable_http_metadata_cache = true").expect("a knob takes a value");
    assert_eq!(rows(&db, value), vec![vec![text("true")]]);
    assert_eq!(
        rows(&db, "SELECT current_setting('enable_http_metadata_cache')"),
        vec![vec![Value::Boolean(true)]]
    );
    db.execute("RESET enable_http_metadata_cache").expect("a knob resets");
    assert_eq!(rows(&db, value), vec![vec![text("false")]]);
    // A number setting reads back as its own type, which is what `typeof` on the pin says too.
    db.execute("SET partitioned_write_max_open_files = 42").expect("a number knob");
    assert_eq!(
        rows(&db, "SELECT current_setting('partitioned_write_max_open_files')"),
        vec![vec![Value::UBigInt(42)]]
    );
    // And a setting that would change what a query returns is the other half of the rule.
    let error = db.execute("SET preserve_insertion_order = false").unwrap_err();
    assert_eq!(error.code().duckdb_name(), "Not implemented Error");
    db.execute("SET preserve_insertion_order = true").expect("the value it already behaves as");
}

/// A pragma that is a statement writes the setting it stands for, and the name carries the value.
///
/// Measured against the pin by snapshotting `duckdb_settings()` either side of each one, which is
/// the only way to fill that table in: `PRAGMA disable_print_progress_bar` writes a setting called
/// `enable_progress_bar_print` and `PRAGMA enable_profiling` writes a word into a `VARCHAR` rather
/// than true into a boolean.
#[test]
fn a_pragma_that_is_a_statement_writes_the_setting_it_stands_for() {
    let db = database();
    let text = |value: &str| Value::Varchar(value.to_string());
    let optimizer = "SELECT value FROM duckdb_settings() WHERE name = 'enable_optimizer'";
    assert_eq!(rows(&db, optimizer), vec![vec![text("true")]]);
    db.execute("PRAGMA disable_optimizer").expect("a pragma that is a statement");
    assert_eq!(rows(&db, optimizer), vec![vec![text("false")]]);
    db.execute("PRAGMA enable_optimizer").expect("and back");
    assert_eq!(rows(&db, optimizer), vec![vec![text("true")]]);
    db.execute("PRAGMA disable_print_progress_bar").expect("the one whose name is not the setting");
    assert_eq!(
        rows(&db, "SELECT current_setting('enable_progress_bar_print')"),
        vec![vec![Value::Boolean(false)]]
    );
    db.execute("PRAGMA enable_profiling").expect("a word rather than a boolean");
    assert_eq!(
        rows(&db, "SELECT current_setting('enable_profiling')"),
        vec![vec![text("query_tree")]]
    );
    // And the pair that turns it off puts it back to nothing rather than to the empty string.
    db.execute("PRAGMA disable_profile").expect("the other spelling of the same statement");
    assert_eq!(rows(&db, "SELECT current_setting('enable_profiling')"), vec![vec![Value::Null]]);
}

/// Nine of the nineteen change nothing a query can see, and succeeding is the whole of what they do.
///
/// Four are deprecated upstream and say so in a warning while doing nothing, and the other five
/// move a flag on the database that `duckdb_settings()` does not list. 692 corpus records are
/// charged to `disable_checkpoint_on_shutdown` alone, all of them a file that says something about
/// checkpoints in its preamble and then goes on to test something else.
#[test]
fn a_pragma_that_changes_nothing_a_query_can_see_still_succeeds() {
    let db = database();
    for statement in [
        "PRAGMA disable_checkpoint_on_shutdown",
        "PRAGMA enable_checkpoint_on_shutdown",
        "PRAGMA disable_object_cache",
        "PRAGMA enable_object_cache",
        "PRAGMA disable_verification",
        "PRAGMA enable_verification",
        "PRAGMA disable_verify_parallelism",
        "PRAGMA verify_parallelism",
        "PRAGMA force_checkpoint",
    ] {
        db.execute(statement).unwrap_or_else(|error| panic!("{statement}: {error}"));
    }
    // A name of the same shape that no engine has is the catalog's complaint, in the words it uses
    // about a pragma rather than the words it uses about a setting.
    let error = db.execute("PRAGMA enable_nothing_at_all").unwrap_err();
    assert_eq!(error.code().duckdb_name(), "Catalog Error");
    assert_eq!(
        error.to_string(),
        "Catalog Error: Pragma Function with name enable_nothing_at_all does not exist!"
    );
}

/// Three settings are unset on a fresh connection rather than empty, and null is what they read as.
#[test]
fn a_setting_the_pin_leaves_unset_reads_as_null_rather_than_as_the_empty_string() {
    let db = database();
    let unset = "SELECT name FROM duckdb_settings() WHERE value IS NULL ORDER BY name";
    let text = |value: &str| Value::Varchar(value.to_string());
    assert_eq!(
        rows(&db, unset),
        vec![
            vec![text("enable_profiling")],
            vec![text("operator_memory_limit")],
            vec![text("parquet_prefetch_column_gap")],
        ]
    );
    // The one that is a number is the reason this matters more than a rendering detail. Reading it
    // as its own type went through the empty string and came out an internal error.
    assert_eq!(
        rows(&db, "SELECT current_setting('parquet_prefetch_column_gap')"),
        vec![vec![Value::Null]]
    );
    db.execute("SET parquet_prefetch_column_gap = 64").expect("and it still takes a number");
    assert_eq!(
        rows(&db, "SELECT current_setting('parquet_prefetch_column_gap')"),
        vec![vec![Value::UBigInt(64)]]
    );
}

/// Every pragma in the catalog is one the parser sends to the catalog.
///
/// The parser decides which pragmas are a statement by their spelling and the catalog decides what
/// each one means, so an entry here that the parser reads as a query would never run at all. This
/// is the join between the two halves, and it is a test rather than a shared list because what it
/// is really checking is that a statement written out reaches the place that answers it.
#[test]
fn every_pragma_the_catalog_has_is_one_the_parser_sends_to_it() {
    let db = database();
    for entry in rudb_functions::PRAGMAS {
        let statement = format!("PRAGMA {}", entry.name);
        db.execute(&statement).unwrap_or_else(|error| panic!("{statement}: {error}"));
    }
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

/// The storage pragma answers in the pin's sixteen columns, and says nothing about rows that have
/// not been written down.
///
/// Both halves matter. The width is the contract a client reads, so it is spelled out here rather
/// than taken from the builder the implementation uses, which would agree with itself whatever it
/// said. The empty answer is the honest one for a table living in memory: the encoder has not run
/// on those rows and will not until a checkpoint, so there is no encoding to report and a row
/// claiming the chunk is stored plain would be a lie. What a real file says is in
/// `tests/storage.rs`, which needs a file to say it.
#[test]
fn the_storage_pragma_answers_in_sixteen_columns_and_leaves_unwritten_rows_out() {
    let db = database();
    let result = db.query("SELECT * FROM pragma_storage_info('t')").expect("the pragma ran");
    assert_eq!(
        result.names(),
        [
            "row_group_id",
            "column_name",
            "column_id",
            "column_path",
            "segment_id",
            "segment_type",
            "start",
            "count",
            "compression",
            "stats",
            "has_updates",
            "persistent",
            "block_id",
            "block_offset",
            "segment_info",
            "additional_block_ids",
        ]
    );
    assert_eq!(
        result.types(),
        [
            LogicalType::BigInt,
            LogicalType::Varchar,
            LogicalType::BigInt,
            LogicalType::Varchar,
            LogicalType::BigInt,
            LogicalType::Varchar,
            LogicalType::BigInt,
            LogicalType::BigInt,
            LogicalType::Varchar,
            LogicalType::Varchar,
            LogicalType::Boolean,
            LogicalType::Boolean,
            LogicalType::BigInt,
            LogicalType::BigInt,
            LogicalType::Varchar,
            LogicalType::list(LogicalType::BigInt),
        ]
    );
    assert_eq!(result.len(), 0, "nothing is on disk here, so nothing is stored in any form");
    // The name goes through the catalog like any other, so a table that is not there says what the
    // catalog says and not something of this function's own.
    assert_eq!(
        failure(&db, "SELECT * FROM pragma_storage_info('nope')"),
        "Table with name nope does not exist!"
    );
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

/// `CALL f(...)` is the table function in a `FROM` clause with the clause left off.
///
/// It is the spelling DuckDB's own extensions are driven with, so a client written against the pin
/// reaches for it, and every table function here was already reachable the long way round. The
/// cases below are one of each shape an argument list comes in rather than a list of functions,
/// since what is being tested is the transform and not the functions, which have their own tests.
#[test]
fn a_call_runs_the_table_function_the_same_query_over_it_would() {
    let db = database();
    for (call, query) in [
        ("CALL pragma_version()", "SELECT * FROM pragma_version()"),
        ("CALL range(3)", "SELECT * FROM range(3)"),
        ("CALL range(1, 7, 2)", "SELECT * FROM range(1, 7, 2)"),
        ("CALL pragma_table_info('t')", "SELECT * FROM pragma_table_info('t')"),
        // A schema in front of the name and the name written in another case, both of which the
        // function form takes and neither of which this spelling has any reason to treat its own
        // way, since it reads the name with the same rule.
        ("CALL main.range(3)", "SELECT * FROM main.range(3)"),
        ("call RANGE(3)", "SELECT * FROM range(3)"),
    ] {
        assert_eq!(rows(&db, call), rows(&db, query), "{call}");
    }
    // One plan, which is the argument for building the query rather than a second path to the same
    // rows: two paths would be two places for the planner to be told something different.
    assert_eq!(rows(&db, "EXPLAIN CALL range(3)"), rows(&db, "EXPLAIN SELECT * FROM range(3)"));
    // A name that is not a table function gets the catalog's words about it, the same ones the
    // other spelling gets, rather than anything this transform says on its own.
    assert_eq!(
        failure(&db, "CALL nope()"),
        "Table Function with name nope does not exist!",
        "the catalog answers for the name"
    );
    assert_eq!(failure(&db, "CALL abs(1)"), failure(&db, "SELECT * FROM abs(1)"));
    // The rule has no room for an alias, on the pin as well, so this is a parser error on both and
    // not a clause that is read and dropped.
    assert!(
        failure(&db, "CALL range(3) AS r").starts_with("syntax error at or near \"AS\""),
        "{}",
        failure(&db, "CALL range(3) AS r")
    );
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

/// The three `show` pragmas, which answer what is here rather than what a query returns.
#[test]
fn the_show_pragmas_list_the_tables_the_databases_and_both_with_the_columns() {
    let db = database();
    db.execute("CREATE VIEW v AS SELECT 1 AS a").expect("a view");
    // Tables and views together and sorted by name, because an unqualified name reaches both and
    // the pin does not say which kind a name is here.
    assert_eq!(
        rows(&db, "PRAGMA show_tables"),
        vec![vec![text("empty")], vec![text("t")], vec![text("v")]]
    );
    assert_eq!(rows(&db, "PRAGMA show_databases"), vec![vec![text("memory")]]);
    let names = |values: Vec<&str>| Value::List {
        element: LogicalType::Varchar,
        values: values.into_iter().map(text).collect(),
    };
    assert_eq!(
        rows(&db, "PRAGMA show_tables_expanded"),
        vec![
            vec![
                text("memory"),
                text("main"),
                text("empty"),
                names(vec!["x"]),
                names(vec!["INTEGER"]),
                Value::Boolean(false),
            ],
            vec![
                text("memory"),
                text("main"),
                text("t"),
                names(vec!["x", "s"]),
                names(vec!["INTEGER", "VARCHAR"]),
                Value::Boolean(false),
            ],
            vec![
                text("memory"),
                text("main"),
                text("v"),
                names(vec!["a"]),
                names(vec!["INTEGER"]),
                Value::Boolean(false),
            ],
        ]
    );
}

/// `SHOW TABLES` and the rest of the special forms are the three pragmas written another way.
#[test]
fn the_show_statement_answers_the_pragma_of_the_same_name() {
    let db = database();
    for (statement, pragma) in [
        ("SHOW TABLES", "PRAGMA show_tables"),
        ("SHOW tables", "PRAGMA show_tables"),
        ("DESCRIBE TABLES", "PRAGMA show_tables"),
        ("SHOW DATABASES", "PRAGMA show_databases"),
        ("DESCRIBE DATABASES", "PRAGMA show_databases"),
        ("SHOW ALL", "PRAGMA show_tables_expanded"),
        ("SHOW ALL TABLES", "PRAGMA show_tables_expanded"),
        ("DESCRIBE ALL", "PRAGMA show_tables_expanded"),
    ] {
        assert_eq!(rows(&db, statement), rows(&db, pragma), "{statement}");
    }
    // A table of that name does not get it back, because the pin reads the word before the name
    // reaches the catalog, and a qualified name is not the special form at all.
    db.execute("CREATE TABLE tables(a INTEGER)").expect("a table named tables");
    assert_eq!(rows(&db, "SHOW TABLES"), rows(&db, "PRAGMA show_tables"));
    assert_eq!(rows(&db, "DESCRIBE TABLES"), rows(&db, "PRAGMA show_tables"));
    assert_eq!(rows(&db, "DESCRIBE main.tables"), rows(&db, "DESCRIBE SELECT * FROM tables"));
}

/// A pragma only name written where a table goes is a name that is not there.
#[test]
fn a_name_that_exists_only_after_the_word_pragma_is_not_a_table_function() {
    let db = database();
    for name in ["pragma_show_tables", "pragma_show_databases", "pragma_show_tables_expanded"] {
        assert_eq!(
            failure(&db, &format!("SELECT * FROM {name}()")),
            format!("Table Function with name {name} does not exist!")
        );
    }
    // The other spelling of the same name works, so the two halves really are separate.
    assert!(!rows(&db, "PRAGMA show_tables").is_empty());
    // And these take no arguments, which is said about the pragma and not about the rewrite.
    let message = failure(&db, "PRAGMA show_tables(1)");
    assert!(message.contains("'show_tables(INTEGER)'"), "{message}");
    assert!(message.ends_with("\tPRAGMA \"show_tables\"\n"), "{message}");
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
    // A vector and a half, worked out from the constant rather than written down, so that the row
    // count stays on the far side of the boundary whatever the boundary is. This test was 2,500
    // rows and stopped crossing anything the day the vector went past that.
    let total = VECTOR_SIZE + VECTOR_SIZE / 2;
    let mut rows = Vec::new();
    for x in 0..i32::try_from(total).unwrap() {
        rows.push(vec![Value::Integer(x)]);
    }
    db.append("big", &rows).unwrap();

    let first = i32::try_from(VECTOR_SIZE).unwrap();
    let last = i32::try_from(total).unwrap() - 1;
    let result = db.query("SELECT x FROM big").unwrap();
    assert_eq!(result.len(), total);
    assert!(result.chunks().len() > 1);
    assert_eq!(result.value_at(0, 0), integer(0));
    assert_eq!(result.value_at(VECTOR_SIZE, 0), integer(first));
    assert_eq!(result.value_at(total - 1, 0), integer(last));
    assert_eq!(result.row(total), None);
    assert_eq!(result.value_at(total, 0), Value::Null);
    assert_eq!(db.value("SELECT count(*) FROM big").unwrap(), Value::BigInt(total as i64));
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
    let error = db.query("SELECT a // 0 FROM z").expect_err("integer division by zero raises");
    assert_eq!(error.span(), Some(Span::new(7, 13)));
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
        "CREATE TABLE u (a INTEGER REFERENCES t (a))",
        "INSERT INTO t (a, a) VALUES (1, 2)",
        "CREATE TABLE u (a INTEGER, a VARCHAR)",
    ] {
        let message = refusal(&db, statement);
        assert!(!message.is_empty(), "{statement} was accepted");
    }
    assert!(db.with_catalog(|catalog| catalog.tables().all(|table| table.name().table != "u")));
}

/// A temporary table goes in the `temp` database and a bare name finds it before the stored one.
///
/// Two tables of one name exist at the same time here, which is the whole of what `temp` is for.
/// The `CREATE` writes into `memory` because a create that did not say otherwise always does, and
/// every read after it goes to `temp` because that is the front of the search path, so the two
/// directions genuinely disagree and the pin disagrees the same way.
#[test]
fn a_temporary_table_shadows_a_stored_one_of_the_same_name() {
    let db = scripted(&[
        "CREATE TABLE t (a INTEGER)",
        "INSERT INTO t VALUES (99)",
        "CREATE TEMPORARY TABLE t (a INTEGER)",
        "INSERT INTO t VALUES (1)",
    ]);
    assert_eq!(rows(&db, "SELECT a FROM t"), vec![vec![integer(1)]]);
    assert_eq!(rows(&db, "SELECT a FROM temp.t"), vec![vec![integer(1)]]);
    assert_eq!(rows(&db, "SELECT a FROM temp.main.t"), vec![vec![integer(1)]]);
    assert_eq!(rows(&db, "SELECT a FROM memory.main.t"), vec![vec![integer(99)]]);
    // Two parts reading as a schema and a table, and `main` is a schema of `temp` before it is a
    // schema of `memory`, so this one is the temporary table as well.
    assert_eq!(rows(&db, "SELECT a FROM main.t"), vec![vec![integer(1)]]);
}

/// The one that is dropped is the one a bare name finds, so dropping twice leaves neither.
#[test]
fn a_drop_takes_the_temporary_table_first_and_the_stored_one_after() {
    let db = scripted(&[
        "CREATE TABLE t (a INTEGER)",
        "CREATE TEMPORARY TABLE t (a INTEGER)",
        "DROP TABLE t",
    ]);
    assert_eq!(
        rows(&db, "SELECT database_name FROM duckdb_tables() WHERE table_name = 't'"),
        vec![vec![text("memory")]]
    );
    db.execute("DROP TABLE t").expect("the stored one is still there");
    assert_eq!(
        rows(&db, "SELECT database_name FROM duckdb_tables() WHERE table_name = 't'"),
        Vec::<Vec<Value>>::new()
    );
}

/// Everything temporary lands in `temp` whatever way the name was written, and a name that says a
/// different database is refused in the pin's own words.
#[test]
fn a_temporary_name_can_only_say_the_temp_database() {
    let db = scripted(&["CREATE TABLE stored (a INTEGER)"]);
    for statement in [
        "CREATE TEMPORARY TABLE one (a INTEGER)",
        "CREATE TEMPORARY TABLE temp.two (a INTEGER)",
        "CREATE TEMPORARY TABLE main.three (a INTEGER)",
        "CREATE TEMPORARY TABLE temp.main.four (a INTEGER)",
        "CREATE TEMPORARY VIEW five AS SELECT 1 AS a",
        "CREATE TEMPORARY VIEW main.six AS SELECT 1 AS a",
    ] {
        db.execute(statement).unwrap_or_else(|error| panic!("{statement}: {error}"));
    }
    assert_eq!(
        rows(&db, "SELECT count(*) FROM duckdb_tables() WHERE database_name = 'temp'"),
        vec![vec![Value::BigInt(4)]]
    );
    assert_eq!(
        rows(&db, "SELECT count(*) FROM duckdb_views() WHERE database_name = 'temp'"),
        vec![vec![Value::BigInt(2)]]
    );
    let outside = "TEMPORARY table names can *only* use the \"temp\" catalog";
    for statement in [
        "CREATE TEMPORARY TABLE memory.seven (a INTEGER)",
        "CREATE TEMPORARY TABLE memory.main.seven (a INTEGER)",
        "CREATE TEMPORARY TABLE system.main.seven (a INTEGER)",
        "CREATE TEMPORARY VIEW memory.seven AS SELECT 1 AS a",
    ] {
        assert_eq!(refusal(&db, statement), outside, "{statement}");
    }
    // A schema in `temp` other than `main` cannot exist, because `CREATE SCHEMA` cannot put one
    // there, so this reports the schema missing rather than reaching for the `memory` one.
    assert_eq!(
        refusal(&db, "CREATE TEMPORARY TABLE nowhere.eight (a INTEGER)"),
        "Schema with name nowhere does not exist!"
    );
}

/// A create that is not temporary still cannot name `temp`, which is the other half of the rule and
/// a different sentence.
#[test]
fn a_create_that_is_not_temporary_cannot_name_the_temp_database() {
    let db = scripted(&[]);
    let inside = "Only TEMPORARY table names can use the \"temp\" catalog";
    assert_eq!(refusal(&db, "CREATE TABLE temp.a (i INTEGER)"), inside);
    assert_eq!(refusal(&db, "CREATE VIEW temp.b AS SELECT 1"), inside);
}

/// The catalog tables report a temporary entry as temporary and as not internal.
///
/// Those two are the same answer everywhere else and they come apart here: the `temp` database is
/// the engine's, and the table in it is somebody's. Measured off the pin, which prints exactly this
/// pair for a temporary table, a temporary view and the columns of both.
#[test]
fn a_temporary_entry_is_temporary_and_is_not_internal() {
    let db = scripted(&[
        "CREATE TABLE stored (a INTEGER)",
        "CREATE TEMPORARY TABLE tt (b INTEGER)",
        "CREATE TEMPORARY VIEW tv AS SELECT 1 AS c",
    ]);
    assert_eq!(
        rows(
            &db,
            "SELECT table_name, internal, temporary FROM duckdb_tables() ORDER BY table_name"
        ),
        vec![
            vec![text("stored"), Value::Boolean(false), Value::Boolean(false)],
            vec![text("tt"), Value::Boolean(false), Value::Boolean(true)],
        ]
    );
    assert_eq!(
        rows(&db, "SELECT internal, temporary FROM duckdb_views() WHERE view_name = 'tv'"),
        vec![vec![Value::Boolean(false), Value::Boolean(true)]]
    );
    assert_eq!(
        rows(
            &db,
            "SELECT table_name, internal FROM duckdb_columns() \
             WHERE table_name IN ('tt', 'tv') ORDER BY table_name"
        ),
        vec![vec![text("tt"), Value::Boolean(false)], vec![text("tv"), Value::Boolean(false)],]
    );
    // The database holding them is internal all the same, and so is its schema.
    assert_eq!(
        rows(&db, "SELECT internal FROM duckdb_databases() WHERE database_name = 'temp'"),
        vec![vec![Value::Boolean(true)]]
    );
    assert_eq!(
        rows(&db, "SELECT internal FROM duckdb_schemas() WHERE database_name = 'temp'"),
        vec![vec![Value::Boolean(true)]]
    );
}

/// A temporary table is a `LOCAL TEMPORARY` in `information_schema`, and a temporary view is just a
/// view, which is the pin's answer to both.
#[test]
fn information_schema_calls_a_temporary_table_a_local_temporary() {
    let db = scripted(&[
        "CREATE TABLE stored (a INTEGER)",
        "CREATE TEMPORARY TABLE tt (b INTEGER)",
        "CREATE TEMPORARY VIEW tv AS SELECT 1 AS c",
    ]);
    assert_eq!(
        rows(
            &db,
            "SELECT table_catalog, table_name, table_type FROM information_schema.tables \
             ORDER BY table_name"
        ),
        vec![
            vec![text("memory"), text("stored"), text("BASE TABLE")],
            vec![text("temp"), text("tt"), text("LOCAL TEMPORARY")],
            vec![text("temp"), text("tv"), text("VIEW")],
        ]
    );
}

/// The two listings that say what is around report a temporary entry, even though the listing of
/// databases leaves `temp` out.
///
/// Both are the pin's answer and the difference between them is the question being asked. One asks
/// which databases a name can be written into, where `temp` is not one, and the others ask what is
/// there to read, where it is.
#[test]
fn the_listings_show_a_temporary_entry_beside_a_stored_one() {
    let db = scripted(&[
        "CREATE TABLE stored (a INTEGER)",
        "CREATE TEMPORARY TABLE tt (b INTEGER)",
        "CREATE TEMPORARY VIEW tv AS SELECT 1 AS c",
    ]);
    assert_eq!(
        rows(&db, "PRAGMA show_tables"),
        vec![vec![text("stored")], vec![text("tt")], vec![text("tv")],]
    );
    assert_eq!(
        rows(&db, "PRAGMA show_tables_expanded")
            .into_iter()
            .map(|row| (row[0].clone(), row[2].clone(), row[5].clone()))
            .collect::<Vec<_>>(),
        vec![
            (text("memory"), text("stored"), Value::Boolean(false)),
            (text("temp"), text("tt"), Value::Boolean(true)),
            (text("temp"), text("tv"), Value::Boolean(true)),
        ]
    );
    assert_eq!(rows(&db, "PRAGMA show_databases"), vec![vec![text("memory")]]);
}

/// A temporary table built from a query, and a temporary one replaced, neither of which touches the
/// stored table of the same name.
#[test]
fn a_temporary_table_takes_the_clauses_a_stored_one_takes() {
    let db = scripted(&[
        "CREATE TABLE t (a INTEGER)",
        "CREATE TEMPORARY TABLE t AS SELECT 7 AS a, 8 AS b",
    ]);
    assert_eq!(rows(&db, "SELECT a, b FROM t"), vec![vec![integer(7), integer(8)]]);
    db.execute("CREATE OR REPLACE TEMPORARY TABLE t (a INTEGER, b INTEGER, c INTEGER)")
        .expect("replacing the temporary one");
    assert_eq!(
        rows(
            &db,
            "SELECT database_name, column_count FROM duckdb_tables() \
             WHERE table_name = 't' ORDER BY database_name"
        ),
        vec![vec![text("memory"), Value::BigInt(1)], vec![text("temp"), Value::BigInt(3)]]
    );
    db.execute("CREATE TEMPORARY TABLE IF NOT EXISTS t (a INTEGER)").expect("already there");
    assert_eq!(
        refusal(&db, "CREATE TEMPORARY TABLE t (a INTEGER)"),
        "Table with name \"t\" already exists!"
    );
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
fn the_arguments_are_expressions_and_a_column_in_one_is_lateral() {
    let db = database();
    assert_eq!(rows(&db, "SELECT count(*) FROM range(2 + 3)"), vec![vec![Value::BigInt(5)]]);
    // `FROM t, range(t.x)` is LATERAL, and the rows of `t` are 3, 1, 2 and 1, so this is seven.
    // There is nothing underneath a table function for the domain to be pushed into, since its
    // arguments are what produce its rows, so the domain is what the call is made over instead.
    // See `Node::LateralFunction`.
    assert_eq!(rows(&db, "SELECT count(*) FROM t, range(t.x)"), vec![vec![Value::BigInt(7)]]);
    // Only the entries to the left. `range` is the first entry here and there is nothing for `t.x`
    // to name, which is a binder error and not a lateral reference read backwards.
    let message = failure(&db, "SELECT count(*) FROM range(t.x), t");
    assert!(message.contains("t"), "{message}");
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
    let total = VECTOR_SIZE + VECTOR_SIZE / 2;
    let result = db.query(&format!("SELECT range FROM range({total})")).unwrap();
    let batches = result.to_arrow().unwrap();
    assert!(batches.len() > 1, "{total} rows is more than one chunk");
    let rows: usize = batches.iter().map(rudb_arrow::RecordBatch::len).sum();
    assert_eq!(rows, total);
    let bytes: usize = batches.iter().map(|batch| batch.column(0).unwrap().values().len()).sum();
    assert_eq!(bytes, total * 8);
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
    //
    // The condition is `<` rather than `=` because an equality is answered by a lookup now and this
    // test is about the loop. A join that finishes in four hundred milliseconds proves nothing
    // about a clock read at the end of one.
    let db = Database::with_config(Config::new().with_query_timeout(Duration::from_secs(1)));
    db.execute("CREATE TABLE l AS SELECT i AS k FROM range(50000) t(i)").unwrap();
    db.execute("CREATE TABLE r AS SELECT i * 2 AS k FROM range(20000) t(i)").unwrap();
    let started = std::time::Instant::now();
    let error =
        db.query("SELECT count(*) FROM l JOIN r ON l.k < r.k").expect_err("that does not finish");
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
    for name in ["compressed_materialization", "join_filter_pushdown", "common_subexpressions"] {
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
fn a_relationship_is_declared_by_a_setting_and_read_back_from_one() {
    let db = Database::new();
    assert_eq!(db.setting("graph_links").unwrap(), "", "a fresh database declares nothing");

    db.execute(
        "SET graph_links = 'lineitem(l_orderkey) -> orders(o_orderkey), \
         orders(o_custkey) -> customer(c_custkey)'",
    )
    .unwrap();
    let declared = rudb_graph::parse_links(&db.setting("graph_links").unwrap()).unwrap();
    assert_eq!(declared.len(), 2);
    assert_eq!(declared[0].name(), "lineitem(l_orderkey) -> orders(o_orderkey)");
    assert_eq!(
        declared[0].cardinality,
        rudb_graph::Cardinality::Unverified,
        "a declaration says what the author believes and the build says what is true"
    );

    // Refused where the author can see it. A declaration that does not parse would otherwise be
    // carried to a checkpoint that silently built nothing out of it.
    let error = db.execute("SET graph_links = 'lineitem(l_orderkey)'").unwrap_err();
    assert!(error.to_string().contains("child(column) -> parent(column)"), "{error}");
    assert_eq!(
        rudb_graph::parse_links(&db.setting("graph_links").unwrap()).unwrap().len(),
        2,
        "and the declaration that worked is still there"
    );

    db.execute("RESET graph_links").unwrap();
    assert_eq!(db.setting("graph_links").unwrap(), "");
}

#[test]
fn rudb_links_says_what_is_declared_and_what_of_it_is_built() {
    let db = Database::new();
    assert!(rows(&db, "SELECT name FROM rudb_links()").is_empty(), "nothing declared, no rows");

    db.create_table("customer", vec![Field::new("c_custkey", LogicalType::Integer)]).unwrap();
    db.append("customer", &[vec![Value::Integer(1)], vec![Value::Integer(2)]]).unwrap();
    db.execute(
        "SET graph_links = 'orders(o_custkey) -> customer(c_custkey), \
         lineitem(l_orderkey) -> orders(o_orderkey)'",
    )
    .unwrap();

    // The declaration is a row whether or not anything was built, because a reader who cannot tell
    // a relationship nobody declared from one nothing acted on cannot tell which to fix.
    let listed = rows(
        &db,
        "SELECT parent_table, cardinality, key_map, key_map_bytes, link, note FROM rudb_links() \
         ORDER BY parent_table",
    );
    assert_eq!(
        listed,
        vec![
            vec![
                Value::Varchar("customer".into()),
                Value::Varchar("unverified".into()),
                Value::Null,
                Value::Null,
                Value::Null,
                Value::Varchar("no key map is stored".into()),
            ],
            vec![
                Value::Varchar("orders".into()),
                Value::Varchar("unverified".into()),
                Value::Null,
                Value::Null,
                Value::Null,
                Value::Varchar("no table of that name".into()),
            ],
        ],
        "a memory table holds no sections and a table that is not here holds nothing at all"
    );
    assert_eq!(
        rows(
            &db,
            "SELECT child_table, child_key, parent_key FROM rudb_links() WHERE \
             parent_table = 'customer'"
        ),
        vec![vec![
            Value::Varchar("orders".into()),
            Value::Varchar("o_custkey".into()),
            Value::Varchar("c_custkey".into()),
        ]]
    );
}

#[test]
fn a_forward_link_takes_the_monotone_form_only_when_every_child_row_has_a_parent_above_the_last() {
    let path = std::env::temp_dir().join(format!(
        "rudb-graph-forms-{}-{}.rdb",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("the clock advances")
            .as_nanos()
    ));
    let db = Database::open(path.to_str().expect("a UTF-8 temporary path")).unwrap();
    db.execute("CREATE TABLE customer (c_custkey INTEGER)").unwrap();
    db.execute("CREATE TABLE orders (o_custkey INTEGER)").unwrap();
    db.execute("CREATE TABLE returns (r_custkey INTEGER)").unwrap();
    db.execute("INSERT INTO customer SELECT i FROM range(1, 4001) AS r(i)").unwrap();
    // Three orders per customer in customer order, which is the shape the loader gives lineitem
    // over orders, and the one section 3.4 keeps as a bit vector rather than a packed column.
    db.execute("INSERT INTO orders SELECT 1 + (i - 1) / 3 FROM range(1, 10001) AS r(i)").unwrap();
    // The same shape with one row whose key is past the last customer, which refuses the monotone
    // form because the bit vector has no way to say that a child found nothing.
    db.execute("INSERT INTO returns SELECT 1 + (i - 1) / 3 FROM range(1, 10001) AS r(i)").unwrap();
    db.execute("INSERT INTO returns VALUES (9999)").unwrap();
    db.execute(
        "SET graph_links = 'orders(o_custkey) -> customer(c_custkey), \
         returns(r_custkey) -> customer(c_custkey)'",
    )
    .unwrap();
    db.execute("CHECKPOINT").unwrap();

    assert_eq!(
        rows(
            &db,
            "SELECT child_table, cardinality, link, note FROM rudb_links() ORDER BY child_table"
        ),
        vec![
            vec![
                Value::Varchar("orders".into()),
                Value::Varchar("exactly one".into()),
                Value::Varchar("monotone".into()),
                Value::Null,
            ],
            vec![
                Value::Varchar("returns".into()),
                Value::Varchar("at most one".into()),
                Value::Varchar("packed".into()),
                Value::Varchar("some child rows have no parent, so this is not exactly one".into()),
            ],
        ],
        "one unmatched child row costs the relationship both its form and its totality"
    );
    // A form is a representation and not an answer: the join counts the same either way.
    assert_eq!(
        rows(&db, "SELECT count(*) FROM orders o JOIN customer c ON o.o_custkey = c.c_custkey"),
        vec![vec![Value::BigInt(10000)]]
    );
    assert_eq!(
        rows(&db, "SELECT count(*) FROM returns r JOIN customer c ON r.r_custkey = c.c_custkey"),
        vec![vec![Value::BigInt(10000)]]
    );

    drop(db);
    std::fs::remove_file(&path).ok();
}

#[test]
fn rudb_links_says_what_shape_the_relationship_turned_out_to_have() {
    let path = std::env::temp_dir().join(format!(
        "rudb-graph-degrees-{}-{}.rdb",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("the clock advances")
            .as_nanos()
    ));
    let db = Database::open(path.to_str().expect("a UTF-8 temporary path")).unwrap();
    db.execute("CREATE TABLE customer (c_custkey INTEGER)").unwrap();
    db.execute("CREATE TABLE orders (o_custkey INTEGER)").unwrap();
    db.execute("CREATE TABLE skewed (s_custkey INTEGER)").unwrap();
    db.execute("INSERT INTO customer SELECT i FROM range(1, 4001) AS r(i)").unwrap();
    // Even: two and a half orders per customer, every customer reached.
    db.execute("INSERT INTO orders SELECT 1 + (i - 1) / 3 FROM range(1, 10001) AS r(i)").unwrap();
    // Skewed: the same ten thousand children, nine thousand of them on one customer and the rest
    // one each, so a thousand customers have a child and three thousand have none.
    db.execute("INSERT INTO skewed SELECT 1 FROM range(1, 9001) AS r(i)").unwrap();
    db.execute("INSERT INTO skewed SELECT 1 + i FROM range(1, 1001) AS r(i)").unwrap();
    db.execute(
        "SET graph_links = 'orders(o_custkey) -> customer(c_custkey), \
         skewed(s_custkey) -> customer(c_custkey)'",
    )
    .unwrap();
    db.execute("CHECKPOINT").unwrap();

    let held = rows(
        &db,
        "SELECT child_table, degree_max, degree_p99, parent_unique, child_total FROM rudb_links() \
         ORDER BY child_table",
    );
    assert_eq!(
        held,
        vec![
            vec![
                Value::Varchar("orders".into()),
                Value::BigInt(3),
                Value::BigInt(3),
                Value::Boolean(true),
                Value::Boolean(true),
            ],
            // The same ten thousand children over the same four thousand parents, and the tail is
            // three orders of magnitude away. That difference is the whole reason the histogram is
            // stored rather than just the mean, which is 2.5 for both.
            vec![
                Value::Varchar("skewed".into()),
                Value::BigInt(9000),
                Value::BigInt(1),
                Value::Boolean(true),
                Value::Boolean(true),
            ],
        ],
        "the shape of the relationship, not the shape of the declaration"
    );
    // A clustered child gathers a short distance and a scattered one a long distance, which is what
    // the link join decision would otherwise have to guess at plan time. Both of the tables above
    // are clustered, so this needs a third whose keys walk the parent table in strides.
    db.execute("CREATE TABLE scattered (x_custkey INTEGER)").unwrap();
    db.execute("INSERT INTO scattered SELECT 1 + (i * 1237) % 4000 FROM range(1, 10001) AS r(i)")
        .unwrap();
    db.execute(
        "SET graph_links = 'orders(o_custkey) -> customer(c_custkey), \
         scattered(x_custkey) -> customer(c_custkey)'",
    )
    .unwrap();
    db.execute("CHECKPOINT").unwrap();
    let locality = rows(
        &db,
        "SELECT child_table FROM rudb_links() WHERE gather_locality < 1 ORDER BY child_table",
    );
    assert_eq!(
        locality,
        vec![vec![Value::Varchar("orders".into())]],
        "the clustered one gathers inside a cache line and the scattered one does not"
    );
    let far = rows(&db, "SELECT child_table FROM rudb_links() WHERE gather_locality > 1000");
    assert_eq!(far, vec![vec![Value::Varchar("scattered".into())]]);

    drop(db);
    std::fs::remove_file(&path).ok();
}

#[test]
fn a_join_over_a_built_relationship_is_planned_as_a_link_join_and_answers_the_same() {
    let path = std::env::temp_dir().join(format!(
        "rudb-graph-linkjoin-{}-{}.rdb",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("the clock advances")
            .as_nanos()
    ));
    let db = Database::open(path.to_str().expect("a UTF-8 temporary path")).unwrap();
    db.execute("CREATE TABLE customer (c_custkey INTEGER, c_name VARCHAR)").unwrap();
    db.execute("CREATE TABLE orders (o_orderkey INTEGER, o_custkey INTEGER)").unwrap();
    db.execute("INSERT INTO customer SELECT i, 'c' || i FROM range(1, 4001) AS r(i)").unwrap();
    db.execute("INSERT INTO orders SELECT i, 1 + i % 4000 FROM range(1, 10001) AS r(i)").unwrap();
    db.execute("SET graph_links = 'orders(o_custkey) -> customer(c_custkey)'").unwrap();
    db.execute("CHECKPOINT").unwrap();
    // The layer is opt in, per `spec/graph/09-measurement.md` section 9.2 and the default in
    // `rudb_common::rules`. The checkpoint above wrote the link either way, which is the point of
    // the default: a file carries the section and a session decides whether anything reads it.
    db.execute("SET graph_sections = 'on'").unwrap();
    // Every order here has a customer, so the relationship carries both certificates and the join
    // below reads no customer column, which is exactly the join the elimination pass deletes. This
    // test is about which algorithm answers a join, so it turns that pass off and keeps one. The
    // elimination has its own test, and its own switch, which is what section 9.2 asks for.
    db.execute("SET stats_join_elimination = false").unwrap();

    // Four thousand customers fit in any cache there is, so the rule declines them, which is the
    // rule working rather than the pass failing. The setting is what a test uses to ask about the
    // other side of the crossover without writing a parent that really does not fit.
    let sql = "SELECT count(*), sum(o_orderkey) FROM orders JOIN customer ON o_custkey = c_custkey";
    let explained = |db: &Database, sql: &str| match db
        .query(&format!("EXPLAIN {sql}"))
        .expect("the explain ran")
        .value_at(0, 1)
    {
        Value::Varchar(text) => text,
        other => panic!("the plan came back as {other:?}"),
    };
    let plan = explained(&db, sql);
    assert!(!plan.contains("LinkJoin"), "four thousand customers were worth a link");
    // Section 6.7. The reason is on the line, with the two numbers the rule read, so a reader who
    // expected a link join finds out it was the size of the parent and not a missing link.
    assert!(
        plan.contains(
            "[builds a hash table, because the parent is 4000 rows and 16000 bytes \
             projected, which fits in cache]"
        ),
        "the plan does not say why it built a hash table:\n{plan}"
    );

    db.execute("SET graph_cache_bytes = 1").unwrap();
    assert_eq!(db.setting("graph_cache_bytes").unwrap(), "1 bytes");
    let plan = explained(&db, sql);
    assert!(plan.contains("LinkJoin"), "the join was not planned as a link join:\n{plan}");
    assert!(
        plan.contains("[reads the link, because the parent does not fit in cache"),
        "the plan does not say why it read the link:\n{plan}"
    );

    // The whole of section 3.1: the sections change the time and not the answer. The control is
    // the same query in the same process with the pass turned off.
    let linked = rows(&db, sql);
    db.execute("SET disabled_optimizers = 'link_join'").unwrap();
    assert!(!explained(&db, sql).contains("LinkJoin"), "the pass is still on");
    assert_eq!(linked, rows(&db, sql), "the link join answered a different question");
    db.execute("RESET disabled_optimizers").unwrap();

    // Section 9.2, the other switch and the one the ablation actually uses. Turning the pass off
    // leaves the sections readable and stops one rewrite. Turning the sections off takes the whole
    // layer away, and the two have to agree or the ablation is measuring the wrong thing.
    db.execute("SET graph_sections = 'off'").unwrap();
    assert!(!explained(&db, sql).contains("LinkJoin"), "the sections are still being read");
    assert_eq!(linked, rows(&db, sql), "the layer changed an answer rather than a time");
    db.execute("SET graph_sections = 'on'").unwrap();
    assert!(explained(&db, sql).contains("LinkJoin"), "turning the sections back on did nothing");

    db.execute("RESET graph_cache_bytes").unwrap();
    assert_eq!(db.setting("graph_cache_bytes").unwrap(), "8.0 MiB", "reset is the default");

    drop(db);
    std::fs::remove_file(&path).ok();
}

/// The query above joins to the parent and reads no column of it, so the gather never runs.
///
/// That is not a contrived shape, it is the shape a foreign key join takes when the parent is only
/// there to filter, and it is worth having a test of. It is also the reason the link join shipped
/// unable to read a parent column at all: `Parent` lays the parts of a column end to end with
/// `rudb_vector::concat`, which declines anything that is not already flat, and a column written by
/// rudb's own writer is bit packed or dictionary encoded and never flat. So every link join that
/// actually gathered something failed, reporting that it was out of memory when it had twenty
/// gigabytes free.
///
/// It went unnoticed because the parent here is built by `INSERT` and checkpointed inside one
/// session, and the sizes above are small enough that what comes back is flat. `cargo xtask
/// sections` found it on the first run over a TPC-H directory. This is that finding as a test: a
/// parent column of each of the two forms real data arrives in, gathered, with the answer checked
/// against the same query with the layer off.
#[test]
fn a_link_join_gathers_a_parent_column_that_was_stored_in_a_form_that_is_not_flat() {
    let path = std::env::temp_dir().join(format!(
        "rudb-graph-gather-{}-{}.rdb",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("the clock advances")
            .as_nanos()
    ));
    let db = Database::open(path.to_str().expect("a UTF-8 temporary path")).unwrap();
    // `c_grade` is four distinct strings over four thousand rows, which the writer dictionary
    // encodes, and `c_balance` is a small range of integers, which it bit packs. Those are the two
    // forms the probe on a real TPC-H file came back with.
    db.execute("CREATE TABLE customer (c_custkey INTEGER, c_grade VARCHAR, c_balance INTEGER)")
        .unwrap();
    db.execute("CREATE TABLE orders (o_orderkey INTEGER, o_custkey INTEGER)").unwrap();
    db.execute(
        "INSERT INTO customer SELECT i, ['bronze', 'silver', 'gold', 'platinum'][1 + i % 4], \
         100 + i % 50 FROM range(1, 4001) AS r(i)",
    )
    .unwrap();
    db.execute("INSERT INTO orders SELECT i, 1 + i % 4000 FROM range(1, 10001) AS r(i)").unwrap();
    db.execute("SET graph_links = 'orders(o_custkey) -> customer(c_custkey)'").unwrap();
    db.execute("CHECKPOINT").unwrap();
    db.execute("SET graph_sections = 'on'").unwrap();
    // Reopened, because the encoding is what the writer chose and a table still holding the chunks
    // it was inserted as would hand back the flat ones the bug hid behind.
    drop(db);
    let db = Database::open(path.to_str().expect("a UTF-8 temporary path")).unwrap();
    db.execute("SET graph_links = 'orders(o_custkey) -> customer(c_custkey)'").unwrap();
    db.execute("SET graph_sections = 'on'").unwrap();

    let sql = "SELECT c_grade, count(*), sum(c_balance) FROM orders JOIN customer \
               ON o_custkey = c_custkey GROUP BY c_grade ORDER BY c_grade";
    let plan = |db: &Database| match db
        .query(&format!("EXPLAIN {sql}"))
        .expect("the explain ran")
        .value_at(0, 1)
    {
        Value::Varchar(text) => text,
        other => panic!("the plan came back as {other:?}"),
    };

    db.execute("SET graph_sections = 'off'").unwrap();
    let hashed = rows(&db, sql);
    assert!(!hashed.is_empty(), "the control answered nothing, so it is not a control");

    db.execute("SET graph_sections = 'on'").unwrap();
    db.execute("SET graph_cache_bytes = 1").unwrap();
    assert!(
        plan(&db).contains("LinkJoin"),
        "the join was not planned as a link join:\n{}",
        plan(&db)
    );
    // The assertion that would have caught it. Before the fix this was an out of memory error and
    // not a wrong answer, so a comparison of rows would never have seen it either.
    assert_eq!(rows(&db, sql), hashed, "gathering a parent column changed the answer");

    drop(db);
    std::fs::remove_file(&path).ok();
}

#[test]
fn a_checkpoint_builds_a_key_map_over_the_parent_of_every_declared_relationship() {
    let path = std::env::temp_dir().join(format!(
        "rudb-graph-checkpoint-{}-{}.rdb",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("the clock advances")
            .as_nanos()
    ));
    let declaration = "SET graph_links = 'orders(o_custkey) -> customer(c_custkey)'";
    let db = Database::open(path.to_str().expect("a UTF-8 temporary path")).unwrap();
    db.execute("CREATE TABLE customer (c_custkey INTEGER, c_name VARCHAR)").unwrap();
    db.execute("CREATE TABLE orders (o_orderkey INTEGER, o_custkey INTEGER)").unwrap();
    db.execute("INSERT INTO customer SELECT i, 'c' || i FROM range(1, 4001) AS r(i)").unwrap();
    db.execute("INSERT INTO orders SELECT i, 1 + i % 4000 FROM range(1, 10001) AS r(i)").unwrap();
    db.execute(declaration).unwrap();
    db.execute("CHECKPOINT").unwrap();

    let built = rows(
        &db,
        "SELECT cardinality, key_map, key_map_bytes > 0, link, link_bytes > 0, note FROM \
         rudb_links()",
    );
    assert_eq!(
        built,
        vec![vec![
            Value::Varchar("exactly one".into()),
            Value::Varchar("identity".into()),
            Value::Boolean(true),
            Value::Varchar("packed".into()),
            Value::Boolean(true),
            Value::Null,
        ]],
        "a distinct ascending key column maps by subtraction, the second pass stored a forward \
         link beside it, and every child row found a parent through it"
    );
    // The queries the declaration was made for are the ones that have to keep their answers.
    assert_eq!(
        rows(&db, "SELECT count(*) FROM orders o JOIN customer c ON o.o_custkey = c.c_custkey"),
        vec![vec![Value::BigInt(10000)]]
    );
    drop(db);

    // A reader that opens the file again finds the sections, and one that never heard of the
    // setting finds the same rows: section 3.1 says the sections change the time and not the
    // answer.
    let reopened = Database::open(path.to_str().expect("a UTF-8 temporary path")).unwrap();
    assert_eq!(
        rows(&reopened, "SELECT count(*), max(c_custkey) FROM customer"),
        vec![vec![Value::BigInt(4000), Value::Integer(4000)]]
    );
    assert!(
        rows(&reopened, "SELECT name FROM rudb_links()").is_empty(),
        "a declaration is a session's and not the file's"
    );
    reopened.execute(declaration).unwrap();
    assert_eq!(
        rows(&reopened, "SELECT key_map FROM rudb_links()"),
        vec![vec![Value::Varchar("identity".into())]],
        "and the map that was built is read back without being built again"
    );
    drop(reopened);
    std::fs::remove_file(path).expect("the temporary native database is removed");
}

#[test]
fn rudb_links_says_what_a_structure_it_decided_against_would_have_cost() {
    let path = std::env::temp_dir().join(format!(
        "rudb-graph-refused-{}-{}.rdb",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("the clock advances")
            .as_nanos()
    ));
    let db = Database::open(path.to_str().expect("a UTF-8 temporary path")).unwrap();
    db.execute("CREATE TABLE customer (c_custkey INTEGER)").unwrap();
    db.execute("CREATE TABLE orders (o_custkey INTEGER)").unwrap();
    // The keys run one to four thousand twice over, so every customer key has a second row with
    // the same key and the parent side is not a key at all. The build encodes the map before it
    // finds that out, because finding it out is what encoding it is, and then keeps nothing.
    db.execute("INSERT INTO customer SELECT 1 + i % 4000 FROM range(0, 8000) AS r(i)").unwrap();
    db.execute("INSERT INTO orders SELECT 1 + i % 4000 FROM range(0, 10000) AS r(i)").unwrap();
    db.execute("SET graph_links = 'orders(o_custkey) -> customer(c_custkey)'").unwrap();
    db.execute("CHECKPOINT").unwrap();

    let listed = rows(
        &db,
        "SELECT cardinality, key_map, key_map_bytes > 0, link, link_bytes, note FROM rudb_links()",
    );
    assert_eq!(
        listed,
        vec![vec![
            Value::Varchar("unverified".into()),
            Value::Null,
            Value::Boolean(true),
            Value::Null,
            Value::Null,
            Value::Varchar(
                "the key map was measured and not kept, so key_map_bytes is what it would cost"
                    .into()
            ),
        ]],
        "a structure that is not there has no form and still has a size"
    );
    // The size is the point of the row: somebody reading it is deciding whether the structure is
    // worth having, and a null would leave them building it to find out. It is the payload the
    // build actually encoded and not an estimate of one, which for eight thousand unsorted keys is
    // a few bytes each.
    let Value::BigInt(bytes) = rows(&db, "SELECT key_map_bytes FROM rudb_links()")[0][0] else {
        panic!("a size");
    };
    assert!(bytes > 8000, "eight thousand keys do not encode in {bytes} bytes");

    // And the join answers the same question it would have with the map, which is section 3.1.
    assert_eq!(
        rows(&db, "SELECT count(*) FROM orders o JOIN customer c ON o.o_custkey = c.c_custkey"),
        vec![vec![Value::BigInt(20000)]]
    );

    drop(db);
    std::fs::remove_file(&path).ok();
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
fn a_pinned_seam_reads_back_through_current_setting_and_not_only_through_the_api() {
    // `SET` took the pin and `Database::setting` answered for it, and `current_setting` in a query
    // said there is no such parameter, because the binder's fallback knew about the row order
    // declarations, the relationship declarations and the rules and not about the seams. What that
    // cost was a sweep that pinned a seam and had no way to check it was measuring what it asked
    // for, which is the one thing a sweep has to be able to check.
    let db = Database::new();
    assert_eq!(
        rows(&db, "SELECT current_setting('seam.chunk.compaction')"),
        vec![vec![text("default")]],
        "an unpinned seam reads back as the word that would leave it there"
    );

    db.execute("SET seam_chunk_compaction = 'learned-gain'").unwrap();
    assert_eq!(
        rows(&db, "SELECT current_setting('seam.chunk.compaction')"),
        vec![vec![text("learned-gain")]]
    );
    // The same seam under the two other spellings of it, since one pin is one seam.
    assert_eq!(
        rows(&db, "SELECT current_setting('chunk.compaction')"),
        vec![vec![text("learned-gain")]]
    );
    assert_eq!(
        rows(&db, "SELECT current_setting('seam_chunk_compaction')"),
        vec![vec![text("learned-gain")]]
    );
    // And the policy, which is the seam that chooses at the others.
    db.execute("SET seam_policy = 'reference'").unwrap();
    assert_eq!(rows(&db, "SELECT current_setting('seam.policy')"), vec![vec![text("reference")]]);

    // A name no seam has is still an unknown setting rather than a seam at its default.
    let error = db.query("SELECT current_setting('seam.chunk.compactoin')").unwrap_err();
    assert_eq!(error.code().duckdb_name(), "Catalog Error");
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
fn a_name_that_is_not_a_setting_says_so_and_offers_the_nearest_ones() {
    let db = Database::new();
    let error = db.execute("SET bogus = 1").unwrap_err();
    assert_eq!(error.code().duckdb_name(), "Catalog Error");
    assert_eq!(error.message(), "unrecognized configuration parameter \"bogus\"");
    // A near miss gets the name it missed, and not the other hundred and ninety one.
    let error = db.execute("SET thread = 1").unwrap_err();
    assert!(error.message().contains("\"threads\""), "{error}");
    assert!(!error.message().contains("\"memory_limit\""), "{error}");
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

/// The operators of a small query and what each of them reported.
///
/// There is no filter operator in here to read, and there is no way to write one over a stored
/// table any more: a filter sitting directly on a `Get` is applied by the scan and gets no operator
/// of its own, whatever its predicate is. So the scan is both halves of this, and the rows flowing
/// from one operator to the next are read off the projection above it instead.
#[test]
fn a_query_reports_what_every_operator_in_it_did() {
    let db = database();
    let sql = "SELECT x FROM t WHERE x > 1";
    let result = db.query(sql).unwrap();
    let metrics = result.metrics().expect("a query that ran has metrics");
    assert_eq!(metrics.query.sql, sql);
    let scan = operator(metrics, "Scan");
    let project = operator(metrics, "Project");
    assert_eq!(scan.detail.as_deref(), Some("t"), "a scan says what it read");
    assert_eq!(scan.rows_out, 2, "two of the four rows are over one and the scan is the filter");
    assert_eq!(project.rows_in, 2, "what the scan produced is what the one above it was handed");
    assert_eq!(project.rows_out, 2, "a projection keeps every row it is given");
    assert_eq!(project.pipeline, scan.pipeline, "nothing here breaks a pipeline");
    assert!(metrics.timing.execute_ns > 0, "running it took longer than nothing");
    assert!(
        metrics.operators.iter().all(|operator| operator.reference_impl),
        "everything at tier 0 is the reference implementation and the document says so"
    );
    assert_eq!(scan.implementations.len(), 1, "the filter it took in sits on one registered seam");
    assert_eq!(scan.implementations[0].seam, "chunk.compaction");
    assert_eq!(scan.implementations[0].name, "never");
}

/// Pinning a seam under an operator takes the reference marker off that operator and no other.
///
/// What the flag was supposed to do all along and could not, because it was set to true on every
/// operator whatever had run. It is read off the scan because the scan is what applies the filter
/// the compaction seam belongs to, and the seams of a filter that moved down have to move down with
/// it: the scan's row is then the only row in the document, and a seam reported nowhere is a seam
/// nobody can tell ran. Reading the document back to check a pinned compaction strategy answered no
/// on every query whose filter moved into the scan, which is most of ClickBench.
#[test]
fn an_operator_that_was_pinned_off_the_reference_stops_being_marked_as_one() {
    let db = database();
    db.execute("SET seam_chunk_compaction = 'learned-gain'").unwrap();
    let result = db.query("SELECT x FROM t WHERE x > 1").unwrap();
    let metrics = result.metrics().expect("a query that ran has metrics");
    let scan = operator(metrics, "Scan");
    assert!(!scan.reference_impl, "{:?}", scan.implementations);
    assert_eq!(scan.implementations[0].seam, "chunk.compaction");
    assert_eq!(scan.implementations[0].name, "learned-gain");
    assert!(
        operator(metrics, "Project").reference_impl,
        "a projection sits on no registered seam, and pinning one under the scan is not about it"
    );
}

/// The ids of a query with nothing pushed anywhere, which is where they run consecutively.
///
/// A query with a filter over a stored table leaves a gap instead, because the filter node keeps
/// the number the plan gave it and never gets an operator. That is the test below this one. This
/// one is about the promise the numbering makes on its own, so it asks for a plan where every node
/// became an operator. It groups rather than counting the lot, because an ungrouped count is
/// answered out of the stored summary and there is then no scan in the query to number.
#[test]
fn every_operator_has_its_own_id_and_a_parent_is_numbered_before_its_children() {
    let db = database();
    let result = db.query("SELECT count(*) FROM t GROUP BY x").unwrap();
    let metrics = result.metrics().expect("a query that ran has metrics");
    let ids: Vec<u32> = metrics.operators.iter().map(|operator| operator.id).collect();
    assert_eq!(ids, (0..u32::try_from(ids.len()).unwrap()).collect::<Vec<_>>());
    let scan = operator(metrics, "Scan");
    let aggregate = operator(metrics, "Aggregate");
    assert!(aggregate.id < scan.id, "the aggregate is above the scan, so it is numbered first");
}

/// A filter the scan applied itself has no row in the document, and leaves its number unused.
///
/// The numbering comes from the plan and the plan still has the filter node in it, so the operators
/// that were built keep the numbers the plan gave them and the one that was not built leaves a gap.
/// Rising and unique is what the numbers promise, not consecutive, and a reader that wants the node
/// behind an id finds it whether or not its neighbour was built.
#[test]
fn a_filter_the_scan_applied_has_no_operator_and_leaves_its_number_unused() {
    let db = database();
    let result = db.query("SELECT count(*) FROM t WHERE x > 1").unwrap();
    let metrics = result.metrics().expect("a query that ran has metrics");
    assert!(
        !metrics.operators.iter().any(|operator| operator.kind == "Filter"),
        "{:?}",
        metrics.operators
    );
    let ids: Vec<u32> = metrics.operators.iter().map(|operator| operator.id).collect();
    assert!(ids.windows(2).all(|pair| pair[0] < pair[1]), "the ids still rise: {ids:?}");
    assert_eq!(operator(metrics, "Scan").rows_out, 2, "the scan produced the rows that passed");
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
    let probe = operator(metrics, "Probe");
    assert_eq!(
        metrics.pipelines[0].depends_on,
        vec![probe.pipeline],
        "the root waits for the probe"
    );
    assert_eq!(
        metrics.pipelines[usize::try_from(probe.pipeline).unwrap()].depends_on,
        vec![gather.pipeline],
        "the probe waits for the side that is gathered first"
    );
    assert_eq!(gather.rows_in, 4, "the whole right side was gathered");
    // The driving side is not gathered, which is the whole of what the streaming probe bought.
    // Three pipelines either way, and the difference is that the third one now has the scan in it
    // rather than waiting for one to finish before this one starts on its rows.
    let driving = metrics
        .operators
        .iter()
        .filter(|operator| operator.kind == "Scan")
        .find(|scan| scan.pipeline == probe.pipeline)
        .expect("the driving scan is in the probe's own pipeline");
    assert_eq!(driving.rows_out, 4, "every driving row went straight into the probe");
}

/// The name in the profile says which of the two operators answered, because they cost very
/// different things and a profile that called both of them a join would not let anybody tell.
#[test]
fn a_join_no_lookup_answers_says_so_in_the_profile() {
    let db = database();
    let result = db.query("SELECT t.x FROM t JOIN t AS u ON t.x < u.x").unwrap();
    let metrics = result.metrics().expect("a query that ran has metrics");
    operator(metrics, "Join");
    assert!(
        !metrics.operators.iter().any(|operator| operator.kind == "Probe"),
        "a range condition is not a lookup"
    );
}

/// The one edge in the document. Without it a reader can add up what a query moved and cannot
/// check that any of it adds up, because the check is an operator's input against what fed it.
#[test]
fn every_operator_but_the_one_that_answers_names_what_consumed_its_rows() {
    let db = database();
    let result = db.query("SELECT sum(x) FROM (SELECT x FROM t WHERE x > 1) ORDER BY 1").unwrap();
    let metrics = result.metrics().expect("a query that ran has metrics");
    let roots: Vec<u32> = metrics
        .operators
        .iter()
        .filter(|operator| operator.parent.is_none())
        .map(|operator| operator.id)
        .collect();
    assert_eq!(roots, vec![0], "one operator produces the answer and it is the first one");
    for operator in &metrics.operators {
        let Some(parent) = operator.parent else { continue };
        let parent = metrics
            .operators
            .iter()
            .find(|other| other.id == parent)
            .expect("a parent is an operator in the same document");
        assert!(parent.id < operator.id, "a parent is numbered before everything under it");
    }
}

/// A parent id pointing at an operator with no row is worse than no parent id, because a reader
/// walking up the chain stops there and reports a gap it cannot tell from a missing measurement.
/// A node folded into another is what puts a hole in the numbering, so the shapes below are the
/// ones where that happens: a filter taken into a scan, a join, a set operation, a materialised
/// `WITH` and a window.
#[test]
fn no_operator_hangs_under_a_row_that_is_not_there() {
    let db = database();
    for sql in [
        "SELECT x FROM t WHERE x > 1",
        "SELECT sum(x) FROM t WHERE x > 1 GROUP BY s ORDER BY 1",
        "SELECT t.x FROM t JOIN t AS u ON t.x = u.x WHERE t.x > 1",
        "SELECT t.x FROM t LEFT JOIN t AS u ON t.x = u.x",
        "SELECT x FROM t WHERE x IN (SELECT x FROM t WHERE x > 1)",
        "SELECT x FROM t UNION SELECT x FROM t",
        "WITH c AS MATERIALIZED (SELECT x FROM t WHERE x > 1) SELECT x FROM c ORDER BY 1",
        "SELECT x, count(*) OVER (PARTITION BY s) FROM t",
        "SELECT x FROM t ORDER BY 1 LIMIT 2",
        "SELECT a.x FROM t AS a, t AS b WHERE a.x > b.x",
    ] {
        let result = db.query(sql).unwrap_or_else(|error| panic!("{sql} did not run: {error}"));
        let metrics = result.metrics().expect("a query that ran has metrics");
        // A materialisation is filled and read back rather than handed upwards, so it is an
        // operator with nothing above it and a query holding one has two.
        let held = metrics.operators.iter().filter(|one| one.kind.contains("CTE")).count();
        let roots = metrics.operators.iter().filter(|one| one.parent.is_none()).count();
        assert_eq!(roots, 1 + held, "{sql} has one operator the answer is read from");
        for one in &metrics.operators {
            let Some(parent) = one.parent else { continue };
            assert!(
                metrics.operators.iter().any(|other| other.id == parent),
                "{sql}: operator {} hangs under {parent}, which has no row",
                one.id
            );
        }
    }
}

/// The check the harness runs, run here so that a shape which stops adding up is a failing test
/// rather than a suite that refuses to publish a number weeks later.
///
/// Every row an operator was handed came out of the operators under it, so the two counts are the
/// same rows counted at the two ends of one handover and there is no tolerance on it. The shapes
/// that have broken it are the two the operator tree is not the plan tree in: a join whose build
/// side the optimizer turned around, and a cross product, which is called again with what is left of
/// its own output and would otherwise count that as input.
#[test]
fn what_an_operator_was_handed_is_what_the_operators_under_it_produced() {
    let db = database();
    for sql in [
        "SELECT x FROM t WHERE x > 1",
        "SELECT sum(x) FROM t WHERE x > 1 GROUP BY s ORDER BY 1",
        "SELECT t.x FROM t JOIN t AS u ON t.x = u.x WHERE t.x > 1",
        "SELECT t.x FROM t LEFT JOIN t AS u ON t.x = u.x",
        "SELECT t.x FROM t JOIN t AS u ON t.x > u.x AND t.x < u.x + 5",
        "SELECT x FROM t WHERE x IN (SELECT x FROM t WHERE x > 1)",
        "SELECT x FROM t UNION SELECT x FROM t",
        "WITH c AS MATERIALIZED (SELECT x FROM t WHERE x > 1) SELECT x FROM c ORDER BY 1",
        "SELECT x, count(*) OVER (PARTITION BY s) FROM t",
        "SELECT a.x FROM t AS a, t AS b WHERE a.x > b.x",
    ] {
        let result = db.query(sql).unwrap_or_else(|error| panic!("{sql} did not run: {error}"));
        let metrics = result.metrics().expect("a query that ran has metrics");
        for one in &metrics.operators {
            let below: u64 = metrics
                .operators
                .iter()
                .filter(|other| other.parent == Some(one.id))
                .map(|other| other.rows_out)
                .sum();
            if !metrics.operators.iter().any(|other| other.parent == Some(one.id)) {
                continue;
            }
            assert_eq!(
                one.rows_in, below,
                "{sql}: operator {} ({}) was handed {} rows and the operators under it made {below}",
                one.id, one.kind, one.rows_in
            );
        }
    }
}

/// The gathered side feeds the operator that holds it, which is the one place the operator tree is
/// a different shape than the plan. A reader that walked the plan instead would compare the join's
/// input against rows the join never saw.
#[test]
fn the_side_a_join_gathers_hangs_under_the_operator_that_holds_it() {
    let db = database();
    let result = db.query("SELECT t.x FROM t JOIN t AS u ON t.x = u.x").unwrap();
    let metrics = result.metrics().expect("a query that ran has metrics");
    let gather = operator(metrics, "Gather");
    let probe = operator(metrics, "Probe");
    assert_eq!(gather.parent, Some(probe.id), "the held side is handed to the join");
    let held = metrics
        .operators
        .iter()
        .find(|other| other.parent == Some(gather.id))
        .expect("something fills the gather");
    assert_eq!(held.rows_out, gather.rows_in, "and what it produced is what the gather took");
}

/// A join is the one operator with two inputs and one row in the document, so its own row counts
/// the driving side alone. Without this the gathered side, which is the half of the query most of
/// the memory and most of the surprises are in, is not in the document at all.
#[test]
fn a_join_says_what_it_built_and_what_it_chose() {
    let db = database();
    let result = db.query("SELECT t.x FROM t JOIN t AS u ON t.x = u.x").unwrap();
    let metrics = result.metrics().expect("a query that ran has metrics");
    let joined = operator(metrics, "Probe").joined.clone().expect("a join reports what it did");
    assert_eq!(joined.algorithm, rudb_metrics::Algorithm::Hash);
    assert_eq!(joined.build_rows, 4, "the whole gathered side went into the table");
    assert!(joined.build_bytes > 0, "a table that holds four rows cost something to hold them");
    let declined = &joined.declined;
    assert_eq!(declined.len(), 1, "the nested loop was the only other answer");
    assert_eq!(declined[0].algorithm, rudb_metrics::Algorithm::Loop);
    assert!(declined[0].reason.contains("equality"), "{}", declined[0].reason);
}

/// The reason is the point of recording it. A nested loop over two sides is the two multiplied, and
/// the fix is nearly always a condition the binder could not find an equality in, which is a thing
/// somebody reading a slow query has to be told rather than left to guess from the time.
#[test]
fn a_join_with_no_lookup_says_why_it_had_to_walk_every_pair() {
    let db = database();
    let result = db.query("SELECT t.x FROM t JOIN t AS u ON t.x < u.x").unwrap();
    let metrics = result.metrics().expect("a query that ran has metrics");
    let joined = operator(metrics, "Join").joined.clone().expect("a join reports what it did");
    assert_eq!(joined.algorithm, rudb_metrics::Algorithm::Loop);
    assert_eq!(joined.build_rows, 4, "the gathered side is walked once per driving row");
    assert_eq!(joined.declined.len(), 1, "the table was the only other answer");
    assert_eq!(joined.declined[0].algorithm, rudb_metrics::Algorithm::Hash);
    assert!(joined.declined[0].reason.contains("no conjunct"), "{}", joined.declined[0].reason);
}

/// Everything that is not a join leaves the key out rather than writing a null into it, because the
/// rest of the plan is most of the plan and a reader filtering for joins should not have to know
/// which of the null shaped rows are one.
#[test]
fn an_operator_that_is_not_a_join_has_no_join_record() {
    let db = database();
    let result = db.query("SELECT sum(x) FROM t WHERE x > 1").unwrap();
    let metrics = result.metrics().expect("a query that ran has metrics");
    assert!(
        metrics.operators.iter().all(|operator| operator.joined.is_none()),
        "no operator in this query has two inputs"
    );
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
    // Filtered so that the query actually reads the rows. `SELECT count(*) FROM t` is answered out
    // of the table's statistics and has no scan in it to report, which is a different thing for this
    // test to be about.
    let sql = "SELECT count(*) FROM t WHERE x > 1";
    let result = db.query(sql).unwrap();
    let written = result.metrics().expect("a query that ran has metrics").render();
    assert!(written.starts_with("{\n  \"schema\": 1,"), "{written}");
    assert!(written.contains(&format!("\"sql\": \"{sql}\"")), "{written}");
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

/// A schema committed by an earlier session is still loaded as a stream rather than through memory.
///
/// What says whether a table already in the file is in the way of the one being written is how many
/// rows it holds, and one that holds none is not in the way: the generation being written takes its
/// place in the catalog. Before that was asked, the name alone was enough to refuse, so a schema
/// created in one session and loaded in the next took the in-memory path and the load needed memory
/// the size of the table. That is not a corner. `CREATE TABLE` in one statement and `INSERT INTO
/// ... SELECT` in the next is what every loading script writes, ClickBench's included.
///
/// Native rather than in memory is the observable, and it is the whole of the difference. A load
/// that streamed has its rows in the file the moment the statement returns and nothing of the table
/// in memory. A load that did not is holding all of them and reaches the file at the next
/// checkpoint.
#[test]
fn an_insert_into_a_committed_empty_table_streams_into_the_file() {
    let path = std::env::temp_dir().join(format!(
        "rudb-native-empty-reload-{}-{}.rdb",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("the clock advances")
            .as_nanos()
    ));
    let database = Database::open(path.to_str().expect("a UTF-8 temporary path"))
        .expect("the new native database opens");
    database.execute("CREATE TABLE hits (id INTEGER, name VARCHAR)").expect("the table is made");
    database.execute("CHECKPOINT").expect("the empty table is committed on its own");
    drop(database);

    let reopened = Database::open(path.to_str().expect("a UTF-8 temporary path"))
        .expect("the committed empty table reopens");
    reopened
        .execute("INSERT INTO hits VALUES (1, 'one'), (2, NULL), (3, 'three')")
        .expect("the rows are inserted");
    assert!(reopened.with_catalog(|catalog| {
        let name = rudb_catalog::QualifiedName::new("memory", "main", "hits");
        catalog.table(&name).expect("the table is there").rows().is_native()
    }));
    drop(reopened);

    let again = Database::open(path.to_str().expect("a UTF-8 temporary path"))
        .expect("the appended generation reopens");
    assert_eq!(
        rows(&again, "SELECT count(*), sum(id), min(name) FROM hits"),
        vec![vec![Value::BigInt(3), Value::HugeInt(6), Value::Varchar("one".into())]]
    );
    std::fs::remove_file(path).expect("the temporary native database is removed");
}

/// The same load with a table beside it in the file, whose rows the new generation carries forward.
///
/// The generation being written names every table the file will hold, so the ones it is not writing
/// are carried by their directory pointer and their pages are not read. The check that they are all
/// there is what keeps a table with rows only in memory from being dropped by a generation that
/// never knew about it, and taking the empty target out of the file's side of that comparison is
/// the part of this that could go wrong quietly.
#[test]
fn a_committed_empty_table_streams_beside_a_table_that_holds_rows() {
    let path = std::env::temp_dir().join(format!(
        "rudb-native-empty-neighbour-{}-{}.rdb",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("the clock advances")
            .as_nanos()
    ));
    let database = Database::open(path.to_str().expect("a UTF-8 temporary path"))
        .expect("the new native database opens");
    database.execute("CREATE TABLE kept (id INTEGER)").expect("the neighbour is made");
    database.execute("INSERT INTO kept VALUES (7), (8)").expect("the neighbour gets rows");
    database.execute("CREATE TABLE hits (id INTEGER, name VARCHAR)").expect("the target is made");
    database.execute("CHECKPOINT").expect("both tables are committed");
    drop(database);

    let reopened = Database::open(path.to_str().expect("a UTF-8 temporary path"))
        .expect("the committed pair reopens");
    reopened.execute("INSERT INTO hits VALUES (1, 'one'), (2, 'two')").expect("the rows go in");
    assert!(reopened.with_catalog(|catalog| {
        let name = rudb_catalog::QualifiedName::new("memory", "main", "hits");
        catalog.table(&name).expect("the table is there").rows().is_native()
    }));
    drop(reopened);

    let again = Database::open(path.to_str().expect("a UTF-8 temporary path"))
        .expect("the appended generation reopens");
    assert_eq!(rows(&again, "SELECT count(*) FROM hits"), vec![vec![Value::BigInt(2)]]);
    // The neighbour is the half that a wrong carry would lose, and it is still all there.
    assert_eq!(
        rows(&again, "SELECT count(*), sum(id) FROM kept"),
        vec![vec![Value::BigInt(2), Value::HugeInt(15)]]
    );
    std::fs::remove_file(path).expect("the temporary native database is removed");
}

/// A temporary table never reaches the file, so reopening the file does not find one.
///
/// This is the part that would be quietly wrong rather than loudly wrong: a checkpoint that wrote
/// the temporary table would leave a table in the file that nobody created, and it would come back
/// as a stored table the next time the file was opened.
#[test]
fn a_temporary_table_is_not_written_into_a_file_backed_database() {
    let path = std::env::temp_dir().join(format!(
        "rudb-native-temporary-{}-{}.rdb",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("the clock advances")
            .as_nanos()
    ));
    let database = Database::open(path.to_str().expect("a UTF-8 temporary path"))
        .expect("the new native database opens");
    database.execute("CREATE TABLE kept (id INTEGER)").expect("the stored table is made");
    database.execute("INSERT INTO kept VALUES (1), (2)").expect("the stored rows go in");
    database
        .execute("CREATE TEMPORARY TABLE gone (id INTEGER)")
        .expect("the temporary table is made");
    database.execute("INSERT INTO gone VALUES (3), (4), (5)").expect("the temporary rows go in");
    assert_eq!(rows(&database, "SELECT count(*) FROM gone"), vec![vec![Value::BigInt(3)]]);
    database.execute("CHECKPOINT").expect("the checkpoint writes the stored table only");
    drop(database);

    let reopened = Database::open(path.to_str().expect("a UTF-8 temporary path"))
        .expect("the committed native database reopens");
    assert_eq!(rows(&reopened, "SELECT count(*) FROM kept"), vec![vec![Value::BigInt(2)]]);
    assert_eq!(
        rows(&reopened, "SELECT table_name FROM duckdb_tables() ORDER BY table_name"),
        vec![vec![text("kept")]]
    );
    std::fs::remove_file(path).expect("the temporary native database is removed");
}

#[test]
fn a_native_frequency_synopsis_answers_count_topn() {
    let path = std::env::temp_dir().join(format!(
        "rudb-native-frequency-{}-{}.rdb",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("the clock advances")
            .as_nanos()
    ));
    let database = Database::open(path.to_str().expect("a UTF-8 temporary path"))
        .expect("the new native database opens");
    database.execute("CREATE TABLE hits (id BIGINT)").expect("the table is made");
    database
        .execute(
            "INSERT INTO hits SELECT CASE WHEN range < 1000 THEN 0 WHEN range < 1500 THEN 1 \
             WHEN range < 1750 THEN 2 ELSE range END FROM range(5000)",
        )
        .expect("the rows are inserted");
    assert_eq!(
        rows(&database, "SELECT id, count(*) AS c FROM hits GROUP BY id ORDER BY c DESC LIMIT 3",),
        vec![
            vec![Value::BigInt(0), Value::BigInt(1000)],
            vec![Value::BigInt(1), Value::BigInt(500)],
            vec![Value::BigInt(2), Value::BigInt(250)],
        ]
    );
    assert_eq!(
        rows(
            &database,
            "SELECT id, id - 1, id - 2, count(*) AS c FROM hits \
             GROUP BY id, id - 1, id - 2 ORDER BY c DESC LIMIT 3",
        ),
        vec![
            vec![Value::BigInt(0), Value::BigInt(-1), Value::BigInt(-2), Value::BigInt(1000)],
            vec![Value::BigInt(1), Value::BigInt(0), Value::BigInt(-1), Value::BigInt(500)],
            vec![Value::BigInt(2), Value::BigInt(1), Value::BigInt(0), Value::BigInt(250)],
        ]
    );
    drop(database);
    std::fs::remove_file(path).expect("the temporary native database is removed");
}

#[test]
fn a_join_a_certificate_says_changes_nothing_is_deleted_and_the_row_counts_agree() {
    let path = std::env::temp_dir().join(format!(
        "rudb-graph-eliminate-{}-{}.rdb",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("the clock advances")
            .as_nanos()
    ));
    let db = Database::open(path.to_str().expect("a UTF-8 temporary path")).unwrap();
    db.execute("CREATE TABLE customer (c_custkey INTEGER, c_name VARCHAR)").unwrap();
    db.execute("CREATE TABLE orders (o_orderkey INTEGER, o_custkey INTEGER)").unwrap();
    db.execute("CREATE TABLE returns (r_orderkey INTEGER, r_custkey INTEGER)").unwrap();
    db.execute("INSERT INTO customer SELECT i, 'c' || i FROM range(1, 4001) AS r(i)").unwrap();
    db.execute("INSERT INTO orders SELECT i, 1 + i % 4000 FROM range(1, 10001) AS r(i)").unwrap();
    // The same, and then one row whose customer does not exist, which is what costs a relationship
    // its totality certificate and so its licence to have its join deleted.
    db.execute("INSERT INTO returns SELECT i, 1 + i % 4000 FROM range(1, 10001) AS r(i)").unwrap();
    db.execute("INSERT INTO returns VALUES (99999, 99999)").unwrap();
    db.execute(
        "SET graph_links = 'orders(o_custkey) -> customer(c_custkey), \
         returns(r_custkey) -> customer(c_custkey)'",
    )
    .unwrap();
    db.execute("CHECKPOINT").unwrap();
    db.execute("SET graph_sections = 'on'").unwrap();

    let explained = |sql: &str| match db
        .query(&format!("EXPLAIN {sql}"))
        .expect("the explain ran")
        .value_at(0, 1)
    {
        Value::Varchar(text) => text,
        other => panic!("the plan came back as {other:?}"),
    };

    // Every order has a customer and this reads no column of one, so the join is doing nothing at
    // all to the row set and the plan says so by not having one.
    let counted = "SELECT count(*), sum(o_orderkey) FROM orders \
                   JOIN customer ON o_custkey = c_custkey";
    assert!(!explained(counted).contains("Join"), "{}", explained(counted));
    let answer = rows(&db, counted);
    assert_eq!(answer, vec![vec![Value::BigInt(10000), Value::HugeInt(50_005_000)]]);
    // The control, which is the same query in the same process with the rule off. This is the row
    // count that says the rewrite changed nothing, and it is the one G4's fifth exit criterion
    // asks for.
    db.execute("SET stats_join_elimination = false").unwrap();
    assert!(explained(counted).contains("Join"), "the rule is still on");
    assert_eq!(answer, rows(&db, counted), "the join was doing something after all");
    db.execute("SET stats_join_elimination = true").unwrap();

    // One unmatched child row, so the join drops it and deleting the join would not. Which rows a
    // join drops is the thing a certificate is about, and this relationship has no certificate for
    // it, so the join stays.
    let partial = "SELECT count(*) FROM returns JOIN customer ON r_custkey = c_custkey";
    assert!(explained(partial).contains("Join"), "{}", explained(partial));
    assert_eq!(rows(&db, partial), vec![vec![Value::BigInt(10000)]], "the stray row is dropped");

    // And the outer join over the total relationship is an inner join, which is a cheaper operator
    // answering the same question. The parent's column is read here, and read for its value rather
    // than counted, so the join stays and only its kind changes.
    let outer = "SELECT max(c_name) FROM orders LEFT JOIN customer ON o_custkey = c_custkey";
    let plan = explained(outer);
    assert!(plan.contains("Join INNER"), "nothing was padded, so nothing was preserved:\n{plan}");
    assert_eq!(rows(&db, outer), vec![vec![Value::Varchar("c999".into())]]);
    // The one that is not total keeps its left join, and the difference in the answer is the row
    // the left join pads and the inner join would have dropped.
    let partial_outer = "SELECT count(*) FROM returns LEFT JOIN customer ON r_custkey = c_custkey";
    assert!(explained(partial_outer).contains("Join LEFT"), "{}", explained(partial_outer));
    assert_eq!(rows(&db, partial_outer), vec![vec![Value::BigInt(10001)]]);

    drop(db);
    std::fs::remove_file(&path).ok();
}

#[test]
fn the_conjunct_the_statistics_call_selective_runs_first_and_the_answer_does_not_change() {
    // A thousand distinct keys over two thousand rows, so `k = 7` keeps a thousandth, and a string
    // function over a varchar is the dearest thing in the predicate. The order that puts the
    // equality first asks the function about two rows instead of about two thousand.
    let db = Database::new();
    db.execute("CREATE TABLE t (k INTEGER, s VARCHAR)").unwrap();
    db.execute("INSERT INTO t SELECT i % 1000, 'row' || i FROM range(0, 2000) AS r(i)").unwrap();

    let query = "SELECT count(*) FROM t WHERE upper(s) = 'ROW7' AND k = 7";
    let plan = db.plan(query).unwrap();
    let filter = plan.lines().find(|line| line.contains("Filter")).expect("a filter is planned");
    let (first, second) = filter.split_once(" AND ").expect("two conjuncts are printed");
    assert!(first.contains("#0.0"), "the counted equality goes in front:\n{plan}");
    assert!(second.contains("upper"), "and the string function goes behind it:\n{plan}");

    // The control. Reordering a conjunction is a rewrite that has to answer the same question, and
    // the switch is what lets the same query be asked both ways.
    let answer = rows(&db, query);
    db.execute("SET stats_filter_order = false").unwrap();
    let unordered = db.plan(query).unwrap();
    let written = unordered.lines().find(|line| line.contains("Filter")).expect("still a filter");
    assert!(
        written.split_once(" AND ").expect("two conjuncts").0.contains("upper"),
        "with the rule off the predicate is in the order it was written:\n{unordered}"
    );
    assert_eq!(rows(&db, query), answer, "the order the conjuncts run in is not an answer");
    assert_eq!(answer, vec![vec![Value::BigInt(1)]]);
}

#[test]
fn a_decimal_sum_is_as_wide_as_a_decimal_goes() {
    let db = database();
    let one = |sql: &str| rows(&db, sql);
    assert_eq!(
        one("SELECT sum(99.9::DECIMAL(3,1)) FROM range(100)"),
        vec![vec![Value::Decimal { unscaled: 99_900, width: 38, scale: 1 }]]
    );
    assert_eq!(
        one("SELECT typeof(sum(x)) FROM (SELECT 1.5::DECIMAL(4,1) x)"),
        vec![vec![text("DECIMAL(38,1)")]]
    );
}

#[test]
fn the_aggregates_past_the_first_five_answer_the_way_the_pin_does() {
    let db = database();
    let one = |sql: &str| -> Vec<String> {
        rows(&db, sql)
            .into_iter()
            .flatten()
            .map(|value| match value {
                Value::Varchar(text) => text,
                other => other.to_string(),
            })
            .collect()
    };
    let three = "FROM (VALUES (NULL), (1), (NULL)) t(x)";
    assert_eq!(
        one(&format!(
            "SELECT list(x)::VARCHAR, array_agg(x)::VARCHAR, first(x), last(x), any_value(x) {three}"
        )),
        ["[NULL, 1, NULL]", "[NULL, 1, NULL]", "NULL", "NULL", "1"]
    );
    assert_eq!(
        one("SELECT bool_and(x), bool_or(x) FROM (VALUES (true), (NULL), (false)) t(x)"),
        ["false", "true"]
    );
    assert_eq!(
        one("SELECT bit_and(x), bit_or(x), bit_xor(x) FROM (VALUES (5), (3), (NULL)) t(x)"),
        ["1", "7", "6"]
    );
    assert_eq!(one("SELECT bit_and(x) FROM (VALUES (-1::TINYINT), (7::TINYINT)) t(x)"), ["7"]);
    assert_eq!(one("SELECT product(x) FROM (VALUES (2), (NULL), (3)) t(x)"), ["6.0"]);
    assert_eq!(
        one("SELECT stddev(x), stddev_pop(x), var_samp(x), var_pop(x), variance(x) \
             FROM (VALUES (1), (2), (4)) t(x)"),
        [
            "1.5275252316519465",
            "1.247219128924647",
            "2.333333333333333",
            "1.5555555555555554",
            "2.333333333333333"
        ]
    );
    assert_eq!(
        one("SELECT stddev(1), var_samp(1), var_pop(1), stddev_pop(1)"),
        ["NULL", "NULL", "0.0", "0.0"]
    );
    assert_eq!(
        one("SELECT string_agg(x), string_agg(x, '-'), group_concat(x, ''), string_agg(x, NULL) \
             FROM (VALUES ('a'), (NULL), ('b')) t(x)"),
        ["a,b", "a-b", "ab", "NULL"]
    );
    assert_eq!(
        one("SELECT list(i), first(i), bool_and(i > 0), string_agg(i::VARCHAR), product(i), \
             bit_or(i), stddev(i) FROM range(0) t(i)"),
        ["NULL"; 7]
    );
    assert_eq!(
        one("SELECT typeof(list(1::TINYINT)), typeof(first(1::DECIMAL(4,1))), \
             typeof(bit_and(1::TINYINT)), typeof(bit_or(NULL)), typeof(product(1)), \
             typeof(stddev(1::DECIMAL(4,1))), typeof(string_agg(1.5)), typeof(list(NULL))"),
        [
            "TINYINT[]",
            "DECIMAL(4,1)",
            "TINYINT",
            "BIGINT",
            "DOUBLE",
            "DOUBLE",
            "VARCHAR",
            "\"NULL\"[]"
        ]
    );
    assert_eq!(
        one("SELECT i % 2 AS k, list(i)::VARCHAR, last(i), string_agg(i::VARCHAR, '|') \
             FROM range(6) t(i) GROUP BY k ORDER BY k"),
        ["0", "[0, 2, 4]", "4", "0|2|4", "1", "[1, 3, 5]", "5", "1|3|5"]
    );
    assert_eq!(
        one("SELECT k, list(v) FILTER (WHERE v > 1)::VARCHAR \
             FROM (VALUES (1, 1), (1, 2), (2, 1), (2, NULL)) t(k, v) GROUP BY k ORDER BY k"),
        ["1", "[2]", "2", "NULL"]
    );
    assert_eq!(
        one("SELECT list_aggr([1, 2, 4], 'stddev'), list_aggr(['a', 'b'], 'string_agg', '-')"),
        ["1.5275252316519465", "a-b"]
    );
    assert_eq!(
        one("SELECT list_sum([1, 2]), list_stddev_samp([1, 2, 4]), list_string_agg(['a', 'b']), \
             list_first([NULL, 1]), LIST_BOOL_AND([true]), list_sum([])"),
        ["3", "1.5275252316519465", "a,b", "NULL", "true", "NULL"]
    );
    let refused = |sql: &str| db.query(sql).unwrap_err().to_string();
    assert!(refused("SELECT list_sum([1], 2)").contains(
        "Macro list_sum() does not support the supplied arguments. You might need to add \
             explicit type casts.\nCandidate macros:\n\tlist_sum(l)"
    ));
    assert!(refused("SELECT bit_and(1.5)").contains("bit_and(DECIMAL(2,1))"));
    assert!(refused("SELECT bool_and(1)").contains("bool_and(INTEGER)"));
    assert!(
        refused("SELECT string_agg(x, y) FROM (VALUES ('a', ','), ('b', ';')) t(x, y)")
            .contains("The \"separator\" argument in function \"string_agg\" must be a constant")
    );
}

#[test]
fn an_aggregate_runs_over_the_elements_of_a_list() {
    let db = database();
    let one = |sql: &str| rows(&db, sql);
    assert_eq!(one("SELECT list_aggr([1, 2, 3], 'sum')"), vec![vec![Value::HugeInt(6)]]);
    assert_eq!(
        db.query("SELECT list_aggr([1, 2, 3], 'sum')").unwrap().names(),
        ["list_aggr(list_value(1, 2, 3), 'sum')"]
    );
    assert_eq!(one("SELECT list_aggr([1, 2, 3], 'avg')"), vec![vec![Value::Double(2.0)]]);
    assert_eq!(one("SELECT list_aggr([1, NULL, 3], 'count')"), vec![vec![Value::BigInt(2)]]);
    assert_eq!(one("SELECT list_aggr(['b', 'a'], 'min')"), vec![vec![text("a")]]);
    assert_eq!(one("SELECT list_aggregate([1, 2], 'SUM')"), vec![vec![Value::HugeInt(3)]]);
    assert_eq!(one("SELECT array_aggr([1, 2], 'max')"), vec![vec![integer(2)]]);
    assert_eq!(one("SELECT aggregate([1], 'sum')"), vec![vec![Value::HugeInt(1)]]);
    assert_eq!(one("SELECT list_aggr([1, 2], 's' || 'um')"), vec![vec![Value::HugeInt(3)]]);
    assert_eq!(
        one("SELECT list_aggr([], 'sum'), list_aggr([], 'count'), list_aggr(NULL, 'count')"),
        vec![vec![Value::Null, Value::BigInt(0), Value::Null]]
    );
    assert_eq!(one("SELECT list_aggr(NULL, 'nope')"), vec![vec![Value::Null]]);
    assert_eq!(
        one("SELECT typeof(list_aggr([1, 2]::TINYINT[], 'sum')), \
             typeof(list_aggr([1, 2]::TINYINT[], 'min')), \
             typeof(list_aggr([1.5]::FLOAT[], 'sum')), \
             typeof(list_aggr([1, 2]::DECIMAL(4,1)[], 'sum'))"),
        vec![vec![text("HUGEINT"), text("TINYINT"), text("DOUBLE"), text("DECIMAL(38,1)")]]
    );
    assert_eq!(
        one("SELECT list_aggr(x, 'sum') FROM (VALUES ([1, 2]), (NULL), ([3])) v(x)"),
        vec![vec![Value::HugeInt(3)], vec![Value::Null], vec![Value::HugeInt(3)]]
    );
}

#[test]
fn an_aggregate_over_a_list_is_refused_the_way_the_pin_refuses_it() {
    let db = database();
    let error = |sql: &str| db.query(sql).unwrap_err().to_string();
    assert!(
        error("SELECT list_aggr([1, 2], 'nope')")
            .starts_with("Catalog Error: Aggregate Function with name nope does not exist!")
    );
    assert!(
        error("SELECT list_aggr([1, 2], 'lower')")
            .starts_with("Catalog Error: lower is not an aggregate function")
    );
    assert!(
        error("SELECT list_aggr([1, 2], NULL)")
            .starts_with("Catalog Error: Aggregate Function with name NULL does not exist!")
    );
    assert!(error("SELECT list_aggr([1, 2])").starts_with(
        "Binder Error: No function matches the given name and argument types \
         'list_aggr(INTEGER[])'. You might need to add explicit type casts.\n\tCandidate \
         functions:\n\tlist_aggr(col0 ANY[], col1 VARCHAR, [ANY...]) -> ANY\n"
    ));
    db.execute("CREATE TABLE f AS SELECT 'sum' AS f").unwrap();
    assert!(error("SELECT list_aggr([1, 2], f) FROM f").starts_with(
        "Binder Error: The \"col1\" argument in function \"list_aggr\" must be a constant \
             expression"
    ));
    assert!(error("SELECT list_aggr(['a'], 'sum')").starts_with(
        "Binder Error: No matching aggregate function\nBinder Error: No function matches the \
         given name and argument types 'sum(VARCHAR)'."
    ));
    assert!(error("SELECT list_aggr([1, 2], 'sum', 3)").starts_with(
        "Binder Error: No matching aggregate function\nBinder Error: No function matches the \
         given name and argument types 'sum(INTEGER, INTEGER)'."
    ));
}

#[test]
fn the_list_functions_that_look_inside_a_list_answer_the_way_the_pin_does() {
    let db = database();
    let shown = |sql: &str| {
        let rows = rows(&db, sql);
        rows[0].iter().map(ToString::to_string).collect::<Vec<_>>().join("|")
    };
    assert_eq!(
        shown(
            "SELECT list_position([1, 2, NULL], NULL), list_position([1, 2], 2::BIGINT), \
             typeof(list_position([1], 1)), list_position(NULL, 1), list_position([1.5, 2], 2), \
             array_indexof([3, 4], 4), list_position([1, 2], 3)"
        ),
        "3|2|INTEGER|NULL|2|2|NULL"
    );
    assert_eq!(
        shown(
            "SELECT list_contains([1, 2, NULL], NULL), list_contains([1, 2], 3), \
             list_contains(NULL, 1), list_contains([], 1), list_contains([[1]], [1]), \
             array_has(['a'], 'a')"
        ),
        "NULL|false|NULL|false|true|true"
    );
    assert_eq!(
        shown(
            "SELECT list_has_any([1, 2], [2, 3]), list_has_any([1, NULL], [NULL]), \
             list_has_any(NULL, [1]), list_has_all([1, 2], [1]), list_has_all([1], [NULL]), \
             list_has_all([1], []), list_has_any([], []), list_has_all([1], [1, 2])"
        ),
        "true|false|NULL|true|true|true|false|false"
    );
    assert_eq!(
        shown(
            "SELECT list_distinct([3, 1, 3, NULL, 2]), list_distinct(NULL), list_distinct([]), \
             list_unique([1, 1, NULL, 2]), typeof(list_unique([1])), list_unique(NULL), \
             list_unique([[1], [1], NULL])"
        ),
        "[3, 1, 2]|NULL|[]|2|UBIGINT|NULL|1"
    );
    assert_eq!(
        shown(
            "SELECT list_intersect([1, 2, 2, 3, NULL], [2, 3, 3, NULL]), \
             list_intersect(NULL, [1]), list_intersect([1], NULL), \
             typeof(list_intersect([1::BIGINT], [1]))"
        ),
        "[2, 3]|NULL|[]|BIGINT[]"
    );
    assert_eq!(
        shown(
            "SELECT list_where([1, 2, 3], [true, false, true]), list_where([1, 2], [true]), \
             list_where([1], [true, true]), list_where(NULL, [true]), \
             list_select([10, 20, 30], [3, 1, 5, 0, -1]), list_select([1, NULL], [2, 2]), \
             list_select([1], NULL)"
        ),
        "[1, 3]|[1]|[1, NULL]|NULL|[30, 10, NULL, NULL, NULL]|[NULL, NULL]|NULL"
    );
    assert_eq!(
        shown(
            "SELECT list_reverse([1, 2, NULL]), list_reverse(NULL), typeof(list_reverse(NULL)), \
             flatten([[1, 2], NULL, [3]]), flatten(NULL), flatten([]), flatten([NULL]), \
             typeof(flatten([[1]]))"
        ),
        "[NULL, 2, 1]|NULL|\"NULL\"|[1, 2, 3]|NULL|[]|[]|INTEGER[]"
    );
    assert_eq!(
        shown(
            "SELECT list_resize([1, 2], 3), list_resize([1], 3, 9), list_resize([1, 2, 3], 1), \
             list_resize(NULL, 2), list_resize([1], NULL), list_resize([1], 2, NULL), \
             list_resize([1], 2.7)"
        ),
        "[1, 2, NULL]|[1, 9, 9]|[1]|NULL|[]|[1, NULL]|[1, NULL, NULL]"
    );
    assert_eq!(
        shown(
            "SELECT list_sort([3, NULL, 1, 2]), list_sort([3, NULL, 1], 'DESC'), \
             list_sort([3, NULL, 1], 'asc', 'nulls first'), list_reverse_sort([3, NULL, 1]), \
             list_reverse_sort([3, NULL, 1], 'NULLS FIRST'), list_sort(NULL), list_sort([]), \
             list_sort([1], NULL), array_sort(['b', 'a', NULL, 'C'])"
        ),
        "[1, 2, 3, NULL]|[3, 1, NULL]|[NULL, 1, 3]|[3, 1, NULL]|[NULL, 3, 1]|NULL|[]|NULL|\
         [C, a, b, NULL]"
    );
}

/// Over a column rather than literals, so the calls that have a loop over the whole vector take
/// it, and the answers are the same ones the pin gives row for row.
#[test]
fn the_list_calls_with_a_vector_loop_answer_over_a_column_the_way_they_do_a_row_at_a_time() {
    let db = database();
    db.execute(
        "CREATE TABLE shaped AS SELECT * FROM (VALUES (1, 1, 2, NULL, 'a'), (2, NULL, 5, 6, NULL), \
         (3, 7, 8, 9, 'a string long enough to leave the view'), (4, 2, 2, 2, 'd')) \
         v(id, a, b, c, s)",
    )
    .expect("created");
    db.execute(
        "CREATE TABLE held AS SELECT id, CASE WHEN id = 3 THEN NULL ELSE list_value(a, b, c) END \
         AS l FROM shaped",
    )
    .expect("created");
    let column = |sql: &str| {
        let rows = rows(&db, sql);
        rows.iter().map(|row| row[0].to_string()).collect::<Vec<_>>().join(";")
    };
    assert_eq!(
        column("SELECT list_value(a, b, c) FROM shaped ORDER BY id"),
        "[1, 2, NULL];[NULL, 5, 6];[7, 8, 9];[2, 2, 2]"
    );
    assert_eq!(
        column("SELECT list_value(s, 'k') FROM shaped ORDER BY id"),
        "[a, k];[NULL, k];[a string long enough to leave the view, k];[d, k]"
    );
    assert_eq!(
        column("SELECT list_reverse(l) FROM held ORDER BY id"),
        "[NULL, 2, 1];[6, 5, NULL];NULL;[2, 2, 2]"
    );
    assert_eq!(column("SELECT len(l) FROM held ORDER BY id"), "3;3;NULL;3");
    assert_eq!(column("SELECT list_distinct(l) FROM held ORDER BY id"), "[1, 2];[5, 6];NULL;[2]");
    assert_eq!(column("SELECT list_unique(l) FROM held ORDER BY id"), "2;2;NULL;1");
    // Past the length where a run is checked against what it kept, so the set is what answers.
    let long = ["a, b, c, id"; 9].join(", ");
    assert_eq!(
        column(&format!("SELECT list_unique(list_value({long})) FROM shaped ORDER BY id")),
        "2;3;4;2"
    );
    assert_eq!(column("SELECT array_length(list_value(a)) FROM shaped ORDER BY id"), "1;1;1;1");
    assert_eq!(column("SELECT list_contains(l, 2) FROM held ORDER BY id"), "true;false;NULL;true");
    assert_eq!(column("SELECT list_position(l, 2) FROM held ORDER BY id"), "2;NULL;NULL;1");
    assert_eq!(column("SELECT list_position(l, NULL) FROM held ORDER BY id"), "3;1;NULL;NULL");
    assert_eq!(
        column("SELECT list_sort(l) FROM held ORDER BY id"),
        "[1, 2, NULL];[5, 6, NULL];NULL;[2, 2, 2]"
    );
    assert_eq!(
        column("SELECT list_sort(l, 'DESC', 'NULLS FIRST') FROM held ORDER BY id"),
        "[NULL, 2, 1];[NULL, 6, 5];NULL;[2, 2, 2]"
    );
    assert_eq!(
        column("SELECT list_reverse_sort(l) FROM held ORDER BY id"),
        "[2, 1, NULL];[6, 5, NULL];NULL;[2, 2, 2]"
    );
    assert_eq!(
        column("SELECT list_grade_up(l) FROM held ORDER BY id"),
        "[1, 2, 3];[2, 3, 1];NULL;[1, 2, 3]"
    );
    assert_eq!(
        column("SELECT list_grade_up(l, 'DESC', 'NULLS FIRST') FROM held ORDER BY id"),
        "[3, 2, 1];[1, 3, 2];NULL;[1, 2, 3]"
    );
    assert_eq!(
        column("SELECT array_grade_up(l, 'DESC') FROM held ORDER BY id"),
        "[2, 1, 3];[3, 2, 1];NULL;[1, 2, 3]"
    );
    assert_eq!(column("SELECT contains(l, 2) FROM held ORDER BY id"), "true;false;NULL;true");
    assert_eq!(column("SELECT contains(s, 'a') FROM shaped ORDER BY id"), "true;NULL;true;false");
    let aggregated = |call: &str, answer: &str| {
        assert_eq!(column(&format!("SELECT {call} FROM held ORDER BY id")), answer, "{call}");
    };
    aggregated("list_sum(l)", "3;11;NULL;6");
    aggregated("list_count(l)", "2;2;NULL;3");
    aggregated("list_min(l)", "1;5;NULL;2");
    aggregated("list_max(l)", "2;6;NULL;2");
    aggregated("list_avg(l)", "1.5;5.5;NULL;2.0");
    aggregated("list_first(l)", "1;NULL;NULL;2");
    aggregated("list_last(l)", "NULL;6;NULL;2");
    let ranged = |call: &str, answer: &str| {
        assert_eq!(column(&format!("SELECT {call} FROM shaped ORDER BY id")), answer, "{call}");
    };
    ranged("list_sum(range(a - 1))", "NULL;NULL;15;0");
    ranged("list_count(range(a - 1))", "0;NULL;6;1");
    ranged("list_bit_or(range(a))", "0;NULL;7;1");
    ranged("list_stddev_samp(range(a))", "NULL;NULL;2.160246899469287;0.7071067811865476");
}

#[test]
fn a_boolean_column_casts_to_a_number_as_a_zero_or_a_one() {
    let db = database();
    db.execute("CREATE TABLE flags AS SELECT * FROM (VALUES (true), (false), (NULL), (true)) v(b)")
        .expect("created");
    let answer = rows(
        &db,
        "SELECT sum(b::INTEGER), sum(b::TINYINT), sum(b::DOUBLE), sum(b::FLOAT), \
         sum(b::HUGEINT), max(b::DECIMAL(4, 2)), count(b::UBIGINT) FROM flags",
    );
    let answer: Vec<String> = answer[0].iter().map(ToString::to_string).collect();
    assert_eq!(answer, ["2", "2", "2.0", "2.0", "2", "1.00", "3"]);
}

#[test]
fn range_and_generate_series_as_scalars_answer_with_the_pins_lists() {
    let db = database();
    let row = |sql: &str| rows(&db, sql)[0][0].to_string();
    let error = |sql: &str| db.query(sql).unwrap_err().to_string();
    let answers = [
        ("SELECT range(5)", "[0, 1, 2, 3, 4]"),
        (
            "SELECT range(TIMESTAMP '2020-01-01', TIMESTAMP '2020-01-03', INTERVAL '1 day 12 hours')",
            "[2020-01-01 00:00:00, 2020-01-02 12:00:00]",
        ),
        (
            "SELECT range(TIMESTAMP '2020-01-03', TIMESTAMP '2020-01-01', INTERVAL '-1 day -12 hours')",
            "[2020-01-03 00:00:00, 2020-01-01 12:00:00]",
        ),
        ("SELECT range(2, 5)", "[2, 3, 4]"),
        ("SELECT range(10, 2, -3)", "[10, 7, 4]"),
        ("SELECT range(0, 0)", "[]"),
        ("SELECT range(5, 1)", "[]"),
        ("SELECT range(5, 1, -1)", "[5, 4, 3, 2]"),
        ("SELECT range(1, 5, 0)", "[]"),
        ("SELECT range(-3)", "[]"),
        ("SELECT range(1, 6, 2)", "[1, 3, 5]"),
        ("SELECT typeof(range(5))", "BIGINT[]"),
        ("SELECT range(NULL)", "NULL"),
        ("SELECT range(1, NULL)", "NULL"),
        ("SELECT generate_series(5)", "[0, 1, 2, 3, 4, 5]"),
        ("SELECT generate_series(2, 5)", "[2, 3, 4, 5]"),
        ("SELECT generate_series(10, 2, -3)", "[10, 7, 4]"),
        ("SELECT generate_series(5, 1, -1)", "[5, 4, 3, 2, 1]"),
        ("SELECT generate_series(1, 5, 0)", "[]"),
        ("SELECT generate_series(1, 6, 2)", "[1, 3, 5]"),
        ("SELECT range(9223372036854775807 - 1, 9223372036854775807)", "[9223372036854775806]"),
        (
            "SELECT generate_series(9223372036854775806, 9223372036854775807)",
            "[9223372036854775806, 9223372036854775807]",
        ),
        ("SELECT range(-9223372036854775807, -9223372036854775808, -1)", "[-9223372036854775807]"),
        (
            "SELECT range(DATE '2020-01-01', DATE '2020-01-04', INTERVAL 1 DAY)",
            "[2020-01-01 00:00:00, 2020-01-02 00:00:00, 2020-01-03 00:00:00]",
        ),
        (
            "SELECT generate_series(TIMESTAMP '2020-01-01', TIMESTAMP '2020-01-02', INTERVAL 6 HOUR)",
            "[2020-01-01 00:00:00, 2020-01-01 06:00:00, 2020-01-01 12:00:00, \
             2020-01-01 18:00:00, 2020-01-02 00:00:00]",
        ),
        (
            "SELECT typeof(range(TIMESTAMP '2020-01-01', TIMESTAMP '2020-01-02', INTERVAL 6 HOUR))",
            "TIMESTAMP[]",
        ),
        ("SELECT range(TIMESTAMP '2020-01-01', TIMESTAMP '2020-01-02', INTERVAL 0 HOUR)", "[]"),
        (
            "SELECT range(TIMESTAMP '2020-01-01', TIMESTAMP '2020-03-01', INTERVAL '1 month 1 day')",
            "[2020-01-01 00:00:00, 2020-02-02 00:00:00]",
        ),
    ];
    for (sql, answer) in answers {
        assert_eq!(row(sql), answer, "{sql}");
    }
    assert_eq!(
        error("SELECT range(0, 100000000000)"),
        "Invalid Input Error: Lists larger than 2^32 elements are not supported"
    );
    assert_eq!(
        error(
            "SELECT range(TIMESTAMP '2020-01-01', TIMESTAMP '2020-03-01', INTERVAL '1 month -1 day')"
        ),
        "Invalid Input Error: Interval with mix of negative/positive entries not supported"
    );
    let refused = error("SELECT range(1.5)");
    assert!(refused.contains("\"range\"(col0 BIGINT) -> BIGINT[]"), "{refused}");
    // Over a column the series is built for each row, and the table function of the same name is
    // still what a FROM clause gets.
    db.execute("CREATE TABLE ends AS SELECT * FROM (VALUES (1, 3), (2, NULL), (3, 0)) v(id, e)")
        .expect("created");
    let column: Vec<String> = rows(&db, "SELECT generate_series(e) FROM ends ORDER BY id")
        .iter()
        .map(|r| r[0].to_string())
        .collect();
    assert_eq!(column, ["[0, 1, 2, 3]", "NULL", "[0]"]);
    db.execute(
        "CREATE TABLE stops AS SELECT * FROM (VALUES (1, TIMESTAMP '2020-01-02'), (2, NULL), \
         (3, TIMESTAMP '2019-12-31')) v(id, s)",
    )
    .expect("created");
    let column: Vec<String> = rows(
        &db,
        "SELECT generate_series(TIMESTAMP '2020-01-01', s, INTERVAL 12 HOUR) FROM stops ORDER BY id",
    )
    .iter()
    .map(|r| r[0].to_string())
    .collect();
    assert_eq!(
        column,
        ["[2020-01-01 00:00:00, 2020-01-01 12:00:00, 2020-01-02 00:00:00]", "NULL", "[]"]
    );
    assert_eq!(row("SELECT count(*) FROM range(4)"), "4");
}

#[test]
fn list_grade_up_and_contains_answer_the_way_the_pin_does() {
    let db = database();
    let row = |sql: &str| rows(&db, sql)[0][0].to_string();
    let error = |sql: &str| db.query(sql).unwrap_err().to_string();
    let answers = [
        ("SELECT list_grade_up([3, 1, NULL, 2])", "[2, 4, 1, 3]"),
        ("SELECT list_grade_up([3, 1, NULL, 2], 'DESC')", "[1, 4, 2, 3]"),
        ("SELECT list_grade_up([3, 1, NULL, 2], 'DESC', 'NULLS FIRST')", "[3, 1, 4, 2]"),
        ("SELECT list_grade_up([1, 1, NULL, 0])", "[4, 1, 2, 3]"),
        ("SELECT typeof(list_grade_up([1]))", "BIGINT[]"),
        ("SELECT list_grade_up(NULL)", "NULL"),
        ("SELECT list_grade_up([])", "[]"),
        ("SELECT grade_up(['b', 'a'])", "[2, 1]"),
        ("SELECT contains([1, 2], 2)", "true"),
        ("SELECT contains([1, NULL], NULL)", "NULL"),
        ("SELECT contains(['a'], 'a')", "true"),
        ("SELECT contains([1, 2], 2.5)", "false"),
        ("SELECT contains([[1]], [1])", "true"),
        ("SELECT contains('abc', 'b')", "true"),
        ("SELECT contains('héllo', 'é')", "true"),
        ("SELECT contains('', '')", "true"),
    ];
    for (sql, answer) in answers {
        assert_eq!(row(sql), answer, "{sql}");
    }
    assert_eq!(
        error("SELECT list_grade_up([1], 'bad')"),
        "Not implemented Error: Enum value: unrecognized value \"BAD\" for enum \"OrderType\""
    );
    assert!(
        error("SELECT contains(1, 2)").contains("contains(col0 T[], col1 T) -> BOOLEAN"),
        "{}",
        error("SELECT contains(1, 2)")
    );
}

#[test]
fn the_list_functions_that_look_inside_a_list_refuse_the_way_the_pin_does() {
    let db = database();
    let error = |sql: &str| db.query(sql).unwrap_err().to_string();
    assert!(error("SELECT list_position([1], 'a'::VARCHAR)").starts_with(
        "Binder Error: Cannot deduce template type 'T' in function: 'list_position(T[], T) -> \
         INTEGER'\nType 'T' was inferred to be:\n - 'INTEGER', from first occurrence\n - \
         'VARCHAR', which is incompatible with previously inferred type!"
    ));
    assert!(error("SELECT list_intersect([1], ['1'])").starts_with(
        "Binder Error: Cannot deduce template type 'T' in function: 'list_intersect(T[], T[]) -> \
         T[]'"
    ));
    assert!(
        error("SELECT list_distinct(1)")
            .contains("\n\tCandidate functions:\n\tlist_distinct(col0 T[]) -> T[]\n")
    );
    assert!(error("SELECT list_where([1], [1])").starts_with(
        "Binder Error: No function matches the given name and argument types \
         'list_where(INTEGER[], INTEGER[])'."
    ));
    assert!(error("SELECT list_select([1, 2], [2.9])").starts_with(
        "Binder Error: No function matches the given name and argument types \
         'list_select(INTEGER[], DECIMAL(2,1)[])'."
    ));
    assert!(error("SELECT flatten([1, 2])").starts_with(
        "Binder Error: No function matches the given name and argument types \
         'flatten(INTEGER[])'."
    ));
    assert!(error("SELECT list_where([1, 2], [true, NULL])").starts_with(
        "Invalid Input Error: NULLs are not allowed as list elements in the second input \
         parameter."
    ));
    assert!(error("SELECT list_select([1], [NULL])").starts_with("Invalid Input Error: NULLs"));
    assert_eq!(
        error("SELECT list_resize([1, 2, 3], 4000999999999999999)"),
        "Out of Range Error: Cannot resize vector to 4000999999999999999 rows: maximum allowed \
         vector size is 128.0 GiB"
    );
    assert!(
        error("SELECT list_reverse(1)")
            .starts_with("Binder Error: ARRAY_SLICE can only operate on LISTs and VARCHARs")
    );
    assert!(error("SELECT list_reverse('abc')").starts_with(
        "Not implemented Error: Slice with steps has not been implemented for string types"
    ));
    assert!(error("SELECT list_sort([1], 'up')").starts_with(
        "Not implemented Error: Enum value: unrecognized value \"UP\" for enum \"OrderType\""
    ));
    assert!(error("SELECT list_reverse_sort([1], 'DESC')").starts_with(
        "Not implemented Error: Enum value: unrecognized value \"DESC\" for enum \
         \"OrderByNullType\""
    ));
    assert!(error("SELECT list_sort([1], x) FROM (SELECT 'ASC' AS x)").starts_with(
        "Binder Error: The \"sort_order\" argument in function \"list_sort\" must be a constant \
         expression"
    ));
}

#[test]
fn range_and_generate_series_over_moments_are_tables_the_way_the_pin_has_them() {
    let db = database();
    let column = |sql: &str| {
        let rows = rows(&db, sql);
        rows.iter().map(|row| row[0].to_string()).collect::<Vec<_>>().join(";")
    };
    let error = |sql: &str| db.query(sql).unwrap_err().to_string();
    let dated = "range(DATE '1992-01-01', DATE '1992-10-01', INTERVAL 1 MONTH)";
    assert_eq!(column(&format!("SELECT typeof(range) FROM {dated} LIMIT 1")), "TIMESTAMP");
    assert_eq!(column(&format!("SELECT count(*) FROM {dated}")), "9");
    assert_eq!(
        column("SELECT * FROM range(DATE '1992-01-31', DATE '1992-06-01', INTERVAL 1 MONTH)"),
        "1992-01-31 00:00:00;1992-02-29 00:00:00;1992-03-29 00:00:00;1992-04-29 00:00:00;\
         1992-05-29 00:00:00"
    );
    assert_eq!(
        column(
            "SELECT * FROM generate_series(TIMESTAMP '1992-01-01', \
             TIMESTAMP '1992-01-01 03:00', INTERVAL 1 HOUR)"
        ),
        "1992-01-01 00:00:00;1992-01-01 01:00:00;1992-01-01 02:00:00;1992-01-01 03:00:00"
    );
    assert_eq!(
        column(
            "SELECT typeof(generate_series) FROM generate_series(TIMESTAMPTZ '1992-01-01', \
             TIMESTAMP '1992-01-01 03:00', INTERVAL 1 HOUR) LIMIT 1"
        ),
        "TIMESTAMP WITH TIME ZONE"
    );
    assert_eq!(
        column("SELECT count(*) FROM range(TIMESTAMP '1992-01-01', NULL, INTERVAL 1 HOUR)"),
        "0"
    );
    assert_eq!(
        error(
            "SELECT * FROM range(TIMESTAMP '1992-01-01', TIMESTAMP '1992-01-02', \
             INTERVAL '1 month -1 day')"
        ),
        "Binder Error: RANGE with composite interval that has mixed signs is not supported"
    );
    assert_eq!(
        error(
            "SELECT * FROM range(TIMESTAMP '1992-01-01', TIMESTAMP '1992-01-02', INTERVAL 0 DAY)"
        ),
        "Binder Error: interval cannot be 0!"
    );
    let refused =
        error("SELECT * FROM generate_series(TIMESTAMP '2020-01-01', TIMESTAMP '2020-01-02')");
    assert!(
        refused.contains(
            "'generate_series(TIMESTAMP, TIMESTAMP)'. You might need to add explicit type casts."
        ) && refused.contains("\"generate_series\"(TIMESTAMP, TIMESTAMP, INTERVAL)"),
        "{refused}"
    );
    // Over a column, one call per row, and a null row is no rows.
    db.execute(
        "CREATE TABLE spans AS SELECT * FROM (VALUES (1, TIMESTAMP '2020-01-01 02:00'), \
         (2, NULL), (3, TIMESTAMP '2020-01-01')) v(id, e)",
    )
    .expect("created");
    assert_eq!(
        column(
            "SELECT id || ' ' || r FROM spans, range(TIMESTAMP '2020-01-01', e, INTERVAL 1 HOUR) t(r) \
             ORDER BY id, r"
        ),
        "1 2020-01-01 00:00:00;1 2020-01-01 01:00:00"
    );
    // The alias names the one column, which still answers to its own name as well.
    assert_eq!(column("SELECT r FROM range(3) r"), "0;1;2");
    assert_eq!(column("SELECT r.range FROM range(1, 3) r"), "1;2");
    assert_eq!(column("SELECT i FROM generate_series(1, 2) i"), "1;2");
    assert_eq!(
        column("SELECT column_name FROM (DESCRIBE SELECT * FROM generate_series(1, 2) AS g)"),
        "g"
    );
    assert_eq!(column("SELECT column_name FROM (DESCRIBE SELECT * FROM range(3) t(x))"), "x");
}

/// A braced struct is `struct_pack` on the pin, and `.a`, `['a']` and `struct_extract` all pick a
/// field out of one. Every answer and sentence here was read off the pin.
#[test]
fn struct_literals_build_and_fields_come_back_out_the_way_the_pin_has_them() {
    let db = database();
    let column = |sql: &str| {
        let rows = rows(&db, sql);
        rows.iter().map(|row| row[0].to_string()).collect::<Vec<_>>().join(";")
    };
    let error = |sql: &str| db.query(sql).unwrap_err().to_string();
    assert_eq!(column("SELECT {'a': 1, 'b': 'x'}"), "{'a': 1, 'b': x}");
    assert_eq!(column("SELECT typeof({'a': 1, 'b': 'x'})"), "STRUCT(a INTEGER, b VARCHAR)");
    assert_eq!(column("SELECT {a: 1, \"B\": 2.5}"), "{'a': 1, 'B': 2.5}");
    assert_eq!(column("SELECT {'a': NULL}"), "{'a': NULL}");
    assert_eq!(column("SELECT {'a': 1}.a"), "1");
    assert_eq!(column("SELECT ({'a': 1}).A"), "1");
    assert_eq!(column("SELECT {'a': 1}['a']"), "1");
    assert_eq!(column("SELECT {'a': {'b': 1}}.a.b"), "1");
    assert_eq!(column("SELECT struct_extract({'a': 1, 'b': 2}, 'b')"), "2");
    assert_eq!(
        column("SELECT {'a': 'it''s', 'b': [1, 2], 'c': {'d': NULL}}"),
        "{'a': 'it\\'s', 'b': [1, 2], 'c': {'d': NULL}}"
    );
    assert_eq!(column("SELECT {'it''s': 1}"), "{'it\\'s': 1}");
    assert_eq!(column("SELECT [{'a': 1}, {'a': 2}]"), "[{'a': 1}, {'a': 2}]");
    assert_eq!(column("SELECT {'a': 1, 'b': 2} IS NULL"), "false");
    assert_eq!(
        error("SELECT {'a': 1, 'A': 2}"),
        "Binder Error: Duplicate named argument \"A\" in function call to '\"struct_pack\"'"
    );
    assert_eq!(
        error("SELECT struct_extract({'a': 1, 'b': 2}, 2)"),
        "Binder Error: struct_extract with an integer key can only be used on unnamed structs, \
         use a string key instead"
    );
    // Over a column the struct is built from the columns side by side, and a field of a null row
    // is null even though the field column under it holds a value.
    let packed = "SELECT CASE WHEN i % 3 = 0 THEN NULL ELSE {'n': i, 's': i::VARCHAR} END AS p \
                  FROM range(6) t(i)";
    assert_eq!(
        column(&format!("SELECT p FROM ({packed})")),
        "NULL;{'n': 1, 's': 1};{'n': 2, 's': 2};NULL;{'n': 4, 's': 4};{'n': 5, 's': 5}"
    );
    assert_eq!(column(&format!("SELECT p.n FROM ({packed})")), "NULL;1;2;NULL;4;5");
    assert_eq!(column("SELECT {'n': i, 'm': i * 2}.m FROM range(4) t(i)"), "0;2;4;6");
}

/// Text inside a list, struct or map is quoted only when it would read wrong bare, which is the
/// pin's rule, checked character by character.
#[test]
fn text_inside_a_nested_value_is_quoted_only_when_it_has_to_be() {
    let db = database();
    let column = |sql: &str| {
        let rows = rows(&db, sql);
        rows.iter().map(|row| row[0].to_string()).collect::<Vec<_>>().join(";")
    };
    assert_eq!(
        column(
            "SELECT ['', 'null', 'Null', 'nul', 'a\"b', 'a:b', 'a=b', 'a,b', 'a(b', 'a[b', \
             'a{b', 'a\\b', 'a''b', 'a b', ' a', 'a/b', 'é']"
        ),
        "['', 'null', 'Null', nul, 'a\"b', 'a:b', 'a=b', 'a,b', 'a(b', 'a[b', 'a{b', a\\b, \
         'a\\'b', a b, ' a', a/b, é]"
    );
    assert_eq!(column("SELECT ['a', NULL]"), "[a, NULL]");
    assert_eq!(column("SELECT [chr(39) || chr(92)]"), "['\\'\\\\']");
}

/// A struct casts to another by field name, and two structs meet at the struct of every field
/// either has, which is how a list of differently shaped structs gets one type. Read off the pin.
#[test]
fn structs_cast_and_compare_by_field_name_the_way_the_pin_does() {
    let db = database();
    let column = |sql: &str| {
        let rows = rows(&db, sql);
        rows.iter().map(|row| row[0].to_string()).collect::<Vec<_>>().join(";")
    };
    let error = |sql: &str| db.query(sql).unwrap_err().to_string();
    assert_eq!(column("SELECT {'a': 1, 'b': 2}::STRUCT(a INT, c INT)"), "{'a': 1, 'c': NULL}");
    assert_eq!(column("SELECT {'a': 1, 'b': 2}::STRUCT(A VARCHAR)"), "{'A': 1}");
    assert_eq!(column("SELECT TRY_CAST({'a': 'x'} AS STRUCT(a INT))"), "{'a': NULL}");
    assert_eq!(column("SELECT {'a': [1, 2]}::STRUCT(a VARCHAR[])"), "{'a': [1, 2]}");
    assert_eq!(column("SELECT {'a': 1}::VARCHAR"), "{'a': 1}");
    assert_eq!(
        error("SELECT {'a': 1, 'b': 2}::STRUCT(x INT, y INT)"),
        "Binder Error: STRUCT to STRUCT cast must have at least one matching member, inputs are \
         (STRUCT(a INTEGER, b INTEGER)) and (STRUCT(x INTEGER, y INTEGER))"
    );
    assert_eq!(column("SELECT [{'a': 1}, {'b': 2}]"), "[{'a': 1, 'b': NULL}, {'a': NULL, 'b': 2}]");
    assert_eq!(
        column("SELECT typeof([{'a': 1, 'b': 'x'}, {'b': 'y', 'c': 2.5}])"),
        "STRUCT(a INTEGER, b VARCHAR, c DECIMAL(2,1))[]"
    );
    assert_eq!(column("SELECT [{'a': 1}, {'a': 2.5}]"), "[{'a': 1.0}, {'a': 2.5}]");
    assert_eq!(column("SELECT {'a': 1, 'b': NULL} < {'a': 1, 'b': 2}"), "false");
    assert_eq!(column("SELECT {'a': 1, 'b': NULL} = {'a': 1, 'b': NULL}"), "true");
    assert_eq!(column("SELECT {'a': 2} > {'a': 1}"), "true");
    assert_eq!(column("SELECT {'a': 1} = {'a': 1, 'b': NULL}"), "true");
    assert_eq!(column("SELECT {'a': 1} = {'b': 1}"), "false");
    assert_eq!(column("SELECT min({'a': i}) FROM range(3) t(i)"), "{'a': 0}");
    assert_eq!(
        column("SELECT {'a': i} FROM range(3) t(i) ORDER BY 1 DESC"),
        "{'a': 2};{'a': 1};{'a': 0}"
    );
    assert_eq!(
        column("SELECT DISTINCT {'a': i % 2} FROM range(4) t(i) ORDER BY 1"),
        "{'a': 0};{'a': 1}"
    );
    assert_eq!(
        column(
            "SELECT k.a || ':' || count(*) FROM (SELECT {'a': i % 2} AS k FROM range(4) t(i)) \
             GROUP BY k ORDER BY k"
        ),
        "0:2;1:2"
    );
}

/// `row(...)` and a bracketed list of values are unnamed structs, which the pin calls a TUPLE and
/// prints as one, and `struct_pack(a := 1)` is the named struct `{'a': 1}` is.
#[test]
fn rows_are_unnamed_structs_and_struct_pack_takes_names_the_way_the_pin_does() {
    let db = database();
    let column = |sql: &str| {
        let rows = rows(&db, sql);
        rows.iter().map(|row| row[0].to_string()).collect::<Vec<_>>().join(";")
    };
    let error = |sql: &str| db.query(sql).unwrap_err().to_string();
    assert_eq!(column("SELECT row(1, 'x')"), "(1, x)");
    assert_eq!(column("SELECT row(1)"), "(1,)");
    assert_eq!(column("SELECT (1, 'it''s')"), "(1, 'it\\'s')");
    assert_eq!(column("SELECT typeof(row(1, row(2)))"), "TUPLE(INTEGER, TUPLE(INTEGER))");
    assert_eq!(column("SELECT row(row(1), 2)"), "((1,), 2)");
    assert_eq!(column("SELECT row(i, i + 1) FROM range(2) t(i)"), "(0, 1);(1, 2)");
    assert_eq!(column("SELECT struct_extract(row(i, 2), 1) FROM range(2) t(i)"), "0;1");
    assert_eq!(column("SELECT row(1, 2)[2]"), "2");
    assert_eq!(column("SELECT row(1, 2)::STRUCT(x INT, y INT)"), "{'x': 1, 'y': 2}");
    assert_eq!(column("SELECT row(1, 2)::VARCHAR"), "(1, 2)");
    assert_eq!(
        column("SELECT [row(1, 2), {'a': 3, 'b': 4}]"),
        "[{'a': 1, 'b': 2}, {'a': 3, 'b': 4}]"
    );
    assert_eq!(column("SELECT {'a': 1} = row(1)"), "true");
    assert_eq!(column("SELECT (1, 2) < (1, 3)"), "true");
    assert_eq!(column("SELECT (1, 2) IN ((1, 2), (3, 4))"), "true");
    assert_eq!(
        error("SELECT struct_extract(row(1, 2), 3)"),
        "Binder Error: Key index 3 for struct_extract out of range - expected an index between 1 \
         and 2"
    );
    assert_eq!(
        error("SELECT row(1, 2)::STRUCT(x INT)"),
        "Mismatch Type Error: Type TUPLE(INTEGER, INTEGER) does not match with STRUCT(x INTEGER). \
         Cannot cast STRUCTs of different size"
    );
    assert_eq!(column("SELECT struct_pack(A := 1, b => 'x')"), "{'A': 1, 'b': x}");
    assert_eq!(column("SELECT typeof(struct_pack(a := 1))"), "STRUCT(a INTEGER)");
    assert_eq!(column("SELECT struct_pack(x := 1).x"), "1");
    assert_eq!(column("SELECT struct_pack()"), "{}");
    assert_eq!(
        error("SELECT struct_pack(1, a := 2)"),
        "Binder Error: Need named argument for struct pack, e.g. STRUCT_PACK(a := b)"
    );
    assert_eq!(
        error("SELECT struct_pack(a := 1, 2)"),
        "Binder Error: Positional argument '2' cannot follow named arguments in function call."
    );
    assert_eq!(
        error("SELECT struct_pack(A := 1, a := 2)"),
        "Binder Error: Duplicate named argument \"a\" in function call to '\"struct_pack\"'"
    );
    assert_eq!(
        db.query("SELECT row(1, 2), struct_pack(a := 1)").unwrap().names(),
        ["\"row\"(1, 2)", "struct_pack(a := 1)"]
    );
}

/// `MAP {k: v}` is `map([k], [v])`, and the calls that read a map answer what the pin does,
/// including the ones that step around a null argument.
#[test]
fn maps_build_and_read_back_the_way_the_pin_has_them() {
    let db = database();
    let column = |sql: &str| {
        let rows = rows(&db, sql);
        rows.iter().map(|row| row[0].to_string()).collect::<Vec<_>>().join(";")
    };
    let error = |sql: &str| db.query(sql).unwrap_err().to_string();
    assert_eq!(column("SELECT MAP {1: 'a', 2: 'b'}"), "{1=a, 2=b}");
    assert_eq!(column("SELECT MAP([1, 2], ['a', NULL])"), "{1=a, 2=NULL}");
    assert_eq!(column("SELECT typeof(MAP {'x': 1.5})"), "MAP(VARCHAR, DECIMAL(2,1))");
    assert_eq!(column("SELECT MAP()"), "{}");
    assert_eq!(column("SELECT MAP([1, 2], NULL)"), "NULL");
    assert_eq!(column("SELECT MAP {'a': NULL, 'it''s': 'x y'}"), "{a=NULL, 'it\\'s'=x y}");
    assert_eq!(column("SELECT MAP {1: 'a'}[1]"), "a");
    assert_eq!(column("SELECT MAP {1: 'a'}[3]"), "NULL");
    assert_eq!(column("SELECT MAP {1: 'a'}['1']"), "a");
    assert_eq!(column("SELECT map_extract(MAP {1: 'a'}, 1)"), "[a]");
    assert_eq!(column("SELECT element_at(MAP {1: 'a'}, 5)"), "[]");
    assert_eq!(column("SELECT map_extract(MAP {1: 'a'}, NULL)"), "[]");
    assert_eq!(column("SELECT map_extract_value(MAP {1: 'a'}, 1)"), "a");
    assert_eq!(column("SELECT map_keys(MAP {1: 'a', 2: 'b'})"), "[1, 2]");
    assert_eq!(column("SELECT map_values(MAP {1: 'a', 2: 'b'})"), "[a, b]");
    assert_eq!(
        column("SELECT map_entries(MAP {1: 'a', 2: 'b'})"),
        "[{'key': 1, 'value': a}, {'key': 2, 'value': b}]"
    );
    assert_eq!(column("SELECT map_from_entries([(1, 'a'), (2, 'b')])"), "{1=a, 2=b}");
    assert_eq!(column("SELECT map_from_entries([{'k': 1, 'v': 'a'}])"), "{1=a}");
    assert_eq!(column("SELECT cardinality(MAP {1: 'a', 2: 'b'})"), "2");
    assert_eq!(column("SELECT typeof(cardinality(MAP {1: 'a'}))"), "UBIGINT");
    assert_eq!(column("SELECT map_concat(MAP {1: 'a'}, MAP {1: 'b', 2: 'c'})"), "{1=b, 2=c}");
    assert_eq!(column("SELECT map_concat(MAP {1: 'a'}, NULL)"), "{1=a}");
    assert_eq!(column("SELECT map_contains(MAP {1: 'a'}, 1)"), "true");
    assert_eq!(column("SELECT map_contains_value(MAP {1: 'a'}, 'a')"), "true");
    assert_eq!(column("SELECT map_contains_entry(MAP {1: 'a'}, 1, 'b')"), "false");
    assert_eq!(column("SELECT MAP {1: 'a', 2: 'x'} < MAP {2: 'a'}"), "true");
    assert_eq!(column("SELECT MAP {'a': 1} = MAP {'a': 1}"), "true");
    assert_eq!(
        column(
            "SELECT m::VARCHAR || ':' || count(*) FROM (VALUES (MAP {1: 'a'}), (MAP {1: 'a'}), \
             (MAP {2: 'b'})) t(m) GROUP BY m ORDER BY m"
        ),
        "{1=a}:2;{2=b}:1"
    );
    assert_eq!(column("SELECT MAP([i], [i * 2]) FROM range(2) t(i)"), "{0=0};{1=2}");
    assert_eq!(
        error("SELECT MAP([1, 1], ['a', 'b'])"),
        "Invalid Input Error: Map keys must be unique."
    );
    assert_eq!(error("SELECT MAP {NULL: 1}"), "Invalid Input Error: Map keys can not be NULL.");
    assert_eq!(
        error("SELECT MAP([1], ['a', 'b'])"),
        "Invalid Input Error: The map key list does not align with the map value list."
    );
    assert!(error("SELECT MAP(1, 2)").starts_with("Binder Error: No function matches"));
    assert_eq!(
        db.query("SELECT MAP {1: 'a'}").unwrap().names(),
        ["\"map\"(list_value(1), list_value('a'))"]
    );
    let inner = "(SELECT MAP {'a': 1} m, {'n': MAP {'b': MAP {'x': 5}}} s)";
    assert_eq!(column(&format!("SELECT m.a, m.b FROM {inner}")), "1");
    assert_eq!(column(&format!("SELECT m.b FROM {inner}")), "NULL");
    assert_eq!(column(&format!("SELECT s.n.b.x FROM {inner}")), "5");
    assert_eq!(column("SELECT (MAP {'a': 1}).a"), "1");
    assert_eq!(column("SELECT m.\"1\" FROM (SELECT MAP {1: 1} m)"), "1");
    assert!(error("SELECT m.a FROM (SELECT MAP {1: 1} m)").starts_with("Conversion Error"));
    assert_eq!(db.query(&format!("SELECT m.a FROM {inner}")).unwrap().names(), ["a"]);
}

/// The struct calls that take a struct apart or put two together, with the answers the pin gives.
#[test]
fn struct_helpers_answer_what_the_pin_does() {
    let db = database();
    let column = |sql: &str| {
        let rows = rows(&db, sql);
        rows.iter().map(|row| row[0].to_string()).collect::<Vec<_>>().join(";")
    };
    let error = |sql: &str| db.query(sql).unwrap_err().to_string();
    assert_eq!(column("SELECT struct_keys({'a': 1, 'B': 2})"), "[a, B]");
    assert_eq!(column("SELECT struct_values({'a': 1, 'b': 'x'})"), "(1, x)");
    assert_eq!(
        column("SELECT typeof(struct_values({'a': 1, 'b': 'x'}))"),
        "TUPLE(INTEGER, VARCHAR)"
    );
    assert_eq!(column("SELECT struct_keys(NULL::STRUCT(a INT))"), "NULL");
    assert_eq!(
        column("SELECT struct_insert({'a': 1}, b := 2, c := 'x')"),
        "{'a': 1, 'b': 2, 'c': x}"
    );
    assert_eq!(column("SELECT struct_insert(NULL::STRUCT(a INT), b := 2)"), "{'a': NULL, 'b': 2}");
    assert_eq!(column("SELECT struct_update({'a': 1, 'b': 2}, B := 'x')"), "{'a': 1, 'B': x}");
    assert_eq!(column("SELECT struct_update({'a': 1}, b := 2)"), "{'a': 1, 'b': 2}");
    assert_eq!(column("SELECT struct_update(NULL::STRUCT(a INT), a := 2)"), "{'a': 2}");
    assert_eq!(column("SELECT struct_concat({'a': 1}, {'b': 2})"), "{'a': 1, 'b': 2}");
    assert_eq!(column("SELECT struct_concat(row(1), row(2))"), "(1, 2)");
    assert_eq!(column("SELECT struct_contains(row(1, 2), 2)"), "true");
    assert_eq!(column("SELECT struct_contains(row(1, NULL), NULL)"), "NULL");
    assert_eq!(column("SELECT struct_position(row(1, 2), 2)"), "2");
    assert_eq!(column("SELECT struct_position(row(1, 2), 3)"), "NULL");
    assert_eq!(
        column("SELECT struct_insert({'a': i}, b := i + 1) FROM range(2) t(i)"),
        "{'a': 0, 'b': 1};{'a': 1, 'b': 2}"
    );
    assert_eq!(
        error("SELECT struct_insert({'a': 1}, a := 2)"),
        "Binder Error: Duplicate struct entry name \"\"a\"\""
    );
    assert_eq!(
        error("SELECT struct_concat({'a': 1}, {'a': 2})"),
        "Invalid Input Error: struct_concat: Arguments contain duplicate STRUCT entry \"a\""
    );
    assert_eq!(
        error("SELECT struct_concat({'a': 1}, row(2))"),
        "Invalid Input Error: struct_concat: Cannot mix named and unnamed STRUCTs"
    );
    assert_eq!(
        error("SELECT struct_keys(row(1, 2))"),
        "Invalid Input Error: struct_keys() expects a STRUCT argument"
    );
    assert_eq!(
        error("SELECT struct_contains({'a': 1}, 1)"),
        "Binder Error: \"struct_contains\" can only be used on unnamed structs"
    );
}

/// Text casts to a list, a struct and a map the way the pin splits it, and a map casts to another
/// map and a struct to a map.
#[test]
fn text_and_nested_values_cast_to_lists_structs_and_maps_the_way_the_pin_does() {
    let db = database();
    let column = |sql: &str| {
        let rows = rows(&db, sql);
        rows.iter().map(|row| row[0].to_string()).collect::<Vec<_>>().join(";")
    };
    let error = |sql: &str| db.query(sql).unwrap_err().to_string();
    assert_eq!(column("SELECT '[1,2]'::INT[]"), "[1, 2]");
    assert_eq!(column("SELECT '[[1,2],[3]]'::INT[][]"), "[[1, 2], [3]]");
    assert_eq!(column("SELECT '[1,,2]'::VARCHAR[]"), "[1, '', 2]");
    assert_eq!(column("SELECT '[ null , NULL, \"null\"]'::VARCHAR[]"), "[NULL, NULL, 'null']");
    assert_eq!(column("SELECT TRY_CAST('[1, x]' AS INT[])"), "[1, NULL]");
    assert_eq!(column("SELECT TRY_CAST('[1, 2' AS INT[])"), "NULL");
    assert_eq!(
        error("SELECT '[1, 2'::INT[]"),
        "Conversion Error: Type VARCHAR with value '[1, 2' can't be cast to the destination type \
         INTEGER[]"
    );
    assert_eq!(column("SELECT '{b: 1}'::STRUCT(a INT, b INT)"), "{'a': NULL, 'b': 1}");
    assert_eq!(column("SELECT '(1)'::STRUCT(a INT, b INT)"), "{'a': 1, 'b': NULL}");
    assert_eq!(
        column("SELECT '{a: {b: [1, 2]}}'::STRUCT(a STRUCT(b INT[]))"),
        "{'a': {'b': [1, 2]}}"
    );
    assert_eq!(column("SELECT TRY_CAST('{x: 1}' AS STRUCT(a INT))"), "NULL");
    assert_eq!(column("SELECT '  { a = 1 ,b= 2 }  '::MAP(VARCHAR, INT)"), "{a=1, b=2}");
    assert_eq!(column("SELECT '{a={x=1}}'::MAP(VARCHAR, MAP(VARCHAR, INT))"), "{a={x=1}}");
    assert_eq!(column("SELECT TRY_CAST('{a=1, b=x}' AS MAP(VARCHAR, INT))"), "{a=1, b=NULL}");
    assert_eq!(
        error("SELECT '{a=1, a=2}'::MAP(VARCHAR, INT)"),
        "Invalid Input Error: Map keys must be unique."
    );
    assert_eq!(column("SELECT (MAP {1: 2})::MAP(BIGINT, DOUBLE)"), "{1=2.0}");
    assert_eq!(column("SELECT (MAP {1.5: 2})::MAP(INT, INT)"), "{2=2}");
    assert_eq!(column("SELECT {'a': 1, 'b': 'x'}::MAP(VARCHAR, VARCHAR)"), "{a=1, b=x}");
    assert!(error("SELECT (MAP {'a': 'x'})::MAP(INT, INT)").starts_with("Conversion Error"));
}

#[test]
fn update_and_delete_change_rows_in_place_the_way_the_pin_does() {
    let db = database();
    let table = |db: &Database| {
        let rows = rows(db, "SELECT * FROM t");
        rows.iter()
            .map(|row| row.iter().map(ToString::to_string).collect::<Vec<_>>().join(","))
            .collect::<Vec<_>>()
            .join(";")
    };
    let error = |sql: &str| db.execute(sql).unwrap_err().to_string();
    let fresh = |db: &Database| {
        db.execute("CREATE OR REPLACE TABLE t (a INTEGER NOT NULL, b VARCHAR)").unwrap();
        db.execute("INSERT INTO t VALUES (1, 'x'), (2, NULL), (3, 'z')").unwrap();
    };
    fresh(&db);
    db.execute("UPDATE t SET a = a + 10, b = 'q' WHERE a >= 2").unwrap();
    assert_eq!(table(&db), "1,x;12,q;13,q");
    fresh(&db);
    db.execute("UPDATE t AS x SET a = x.a * 2 WHERE b IS NULL").unwrap();
    assert_eq!(table(&db), "1,x;4,NULL;3,z");
    db.execute("UPDATE t SET b = '7'").unwrap();
    assert_eq!(table(&db), "1,7;4,7;3,7");
    fresh(&db);
    db.execute("DELETE FROM t WHERE b = 'x'").unwrap();
    assert_eq!(table(&db), "2,NULL;3,z");
    db.execute("DELETE FROM t AS x WHERE x.b IS NULL").unwrap();
    assert_eq!(table(&db), "3,z");
    db.execute("DELETE FROM t").unwrap();
    assert_eq!(table(&db), "");
    fresh(&db);
    db.execute("TRUNCATE t").unwrap();
    assert_eq!(table(&db), "");
    fresh(&db);
    assert_eq!(
        error("UPDATE t SET c = 1"),
        "Binder Error: Referenced update column c not found in table!"
    );
    assert_eq!(
        error("UPDATE t SET a = 1, A = 2"),
        "Binder Error: Multiple assignments to same column \"\"A\"\""
    );
    assert_eq!(error("UPDATE t SET a = NULL"), "Constraint Error: NOT NULL constraint failed: t.a");
    assert_eq!(table(&db), "1,x;2,NULL;3,z");
    assert_eq!(
        error("UPDATE t SET t.a = 5"),
        "Parser Error: Qualified column names in UPDATE .. SET not supported"
    );
    db.execute("CREATE VIEW v AS SELECT 1 AS a").unwrap();
    assert_eq!(error("UPDATE v SET a = 2"), "Binder Error: Can only update base table");
}

#[test]
fn a_rollback_puts_back_what_the_transaction_changed_the_way_the_pin_does() {
    let db = Database::new();
    let count = |db: &Database| rows(db, "SELECT count(*) FROM t")[0][0].to_string();
    let error = |sql: &str| db.execute(sql).unwrap_err().to_string();
    db.execute("CREATE TABLE t (a INTEGER NOT NULL)").unwrap();
    db.execute("BEGIN TRANSACTION").unwrap();
    db.execute("INSERT INTO t VALUES (1), (2)").unwrap();
    assert_eq!(count(&db), "2");
    db.execute("ROLLBACK").unwrap();
    assert_eq!(count(&db), "0");
    db.execute("START TRANSACTION").unwrap();
    db.execute("INSERT INTO t VALUES (1), (2)").unwrap();
    db.execute("COMMIT").unwrap();
    assert_eq!(count(&db), "2");
    db.execute("BEGIN").unwrap();
    db.execute("CREATE TABLE u (b INTEGER)").unwrap();
    db.execute("DELETE FROM t").unwrap();
    db.execute("ABORT").unwrap();
    assert_eq!(count(&db), "2");
    assert!(
        error("SELECT * FROM u").starts_with("Catalog Error: Table with name u does not exist")
    );
    assert_eq!(
        error("COMMIT"),
        "TransactionContext Error: cannot commit - no transaction is active"
    );
    assert_eq!(
        error("ROLLBACK"),
        "TransactionContext Error: cannot rollback - no transaction is active"
    );
    db.execute("BEGIN").unwrap();
    assert_eq!(
        error("BEGIN"),
        "TransactionContext Error: cannot start a transaction within a transaction"
    );
    assert_eq!(
        error("SELECT 1"),
        "TransactionContext Error: Current transaction is aborted (please ROLLBACK)"
    );
    db.execute("ROLLBACK").unwrap();
    db.execute("BEGIN").unwrap();
    db.execute("INSERT INTO t VALUES (3)").unwrap();
    assert_eq!(
        error("INSERT INTO t VALUES (NULL)"),
        "Constraint Error: NOT NULL constraint failed: t.a"
    );
    assert_eq!(
        error("SELECT 1"),
        "TransactionContext Error: Current transaction is aborted (please ROLLBACK)"
    );
    db.execute("COMMIT").unwrap();
    assert_eq!(count(&db), "2");
    db.execute("BEGIN").unwrap();
    assert!(db.execute("SELEC 1").is_err());
    db.execute("INSERT INTO t VALUES (3)").unwrap();
    db.execute("END").unwrap();
    assert_eq!(count(&db), "3");
    db.execute("BEGIN READ ONLY").unwrap();
    assert_eq!(
        error("INSERT INTO t VALUES (4)"),
        "TransactionContext Error: Cannot write to database \"\"memory\"\" - transaction is \
         launched in read-only mode"
    );
    db.execute("ROLLBACK").unwrap();
}

#[test]
fn a_file_keeps_what_committed_and_loses_what_rolled_back_or_was_left_open() {
    let path = std::env::temp_dir().join(format!(
        "rudb-transactions-{}-{}.rdb",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("the clock advances")
            .as_nanos()
    ));
    let name = path.to_str().expect("a UTF-8 temporary path");
    let count = |db: &Database| rows(db, "SELECT count(*) FROM t")[0][0].to_string();
    {
        let db = Database::open(name).unwrap();
        db.execute("CREATE TABLE t (a INTEGER)").unwrap();
        db.execute("BEGIN").unwrap();
        db.execute("INSERT INTO t SELECT i FROM range(100) AS r(i)").unwrap();
        db.execute("ROLLBACK").unwrap();
        db.execute("BEGIN").unwrap();
        db.execute("INSERT INTO t VALUES (1), (2)").unwrap();
        db.execute("COMMIT").unwrap();
        db.execute("BEGIN").unwrap();
        db.execute("UPDATE t SET a = 7").unwrap();
        db.execute("INSERT INTO t VALUES (3)").unwrap();
    }
    let db = Database::open(name).unwrap();
    assert_eq!(count(&db), "2");
    assert_eq!(rows(&db, "SELECT sum(a) FROM t")[0][0].to_string(), "3");
    drop(db);
    let _ = std::fs::remove_file(&path);
}

#[test]
fn a_write_answers_with_the_count_of_rows_it_wrote_the_way_the_pin_does() {
    let db = Database::new();
    let count = |sql: &str| {
        let result = db.execute(sql).unwrap();
        assert_eq!(result.names(), ["Count"]);
        assert_eq!(result.types(), [LogicalType::BigInt]);
        assert_eq!(result.text_at(0, 0), result.changes().unwrap().to_string());
        result.changes().unwrap()
    };
    assert!(db.execute("CREATE TABLE t (a INTEGER, b VARCHAR)").unwrap().changes().is_none());
    assert_eq!(count("INSERT INTO t VALUES (1, 'x'), (2, NULL), (3, 'z')"), 3);
    assert_eq!(count("INSERT INTO t SELECT * FROM t WHERE a > 5"), 0);
    assert_eq!(count("UPDATE t SET b = 'q' WHERE a >= 2"), 2);
    assert_eq!(count("UPDATE t SET b = 'q' WHERE b IS NULL"), 0);
    assert_eq!(count("UPDATE t SET a = a + 1"), 3);
    assert_eq!(rows(&db, "SELECT a, b FROM t ORDER BY a").len(), 3);
    assert_eq!(count("DELETE FROM t WHERE a = 2"), 1);
    assert_eq!(count("DELETE FROM t"), 2);
    assert!(db.query("SELECT 1").unwrap().changes().is_none());
}

#[test]
fn returning_answers_with_the_rows_the_statement_wrote_the_way_the_pin_does() {
    let db = Database::new();
    let answer = |sql: &str| {
        let result = db.execute(sql).unwrap();
        assert!(result.changes().is_none(), "{sql} answered with a count");
        (0..result.len())
            .map(|row| {
                (0..result.width()).map(|at| result.text_at(row, at)).collect::<Vec<_>>().join(",")
            })
            .collect::<Vec<_>>()
            .join(";")
    };
    db.execute("CREATE TABLE t (a INTEGER, b VARCHAR)").unwrap();
    assert_eq!(answer("INSERT INTO t VALUES (1, 'x'), (2, 'y') RETURNING *"), "1,x;2,y");
    assert_eq!(answer("INSERT INTO t AS n VALUES (3, 'z') RETURNING n.a * 10, b"), "30,z");
    assert_eq!(answer("UPDATE t SET b = 'q' WHERE a >= 2 RETURNING a, b"), "2,q;3,q");
    assert_eq!(answer("UPDATE t SET a = a WHERE a > 5 RETURNING a"), "");
    assert_eq!(answer("DELETE FROM t WHERE a = 2 RETURNING b, a"), "q,2");
    assert_eq!(answer("SELECT count(*), sum(a) FROM t"), "2,4");
    assert_eq!(answer("DELETE FROM t RETURNING count(*)"), "2");
    assert_eq!(answer("SELECT count(*) FROM t"), "0");
}

#[test]
fn update_from_and_delete_using_change_each_row_once_however_many_rows_match_it() {
    let db = Database::new();
    let answer = |sql: &str| {
        let result = db.execute(sql).unwrap();
        (0..result.len())
            .map(|row| {
                (0..result.width()).map(|at| result.text_at(row, at)).collect::<Vec<_>>().join(",")
            })
            .collect::<Vec<_>>()
            .join(";")
    };
    db.execute("CREATE TABLE t (id INTEGER, v INTEGER)").unwrap();
    db.execute("INSERT INTO t VALUES (1, 0), (2, 0), (3, 0)").unwrap();
    db.execute("CREATE TABLE s (id INTEGER, v INTEGER)").unwrap();
    db.execute("INSERT INTO s VALUES (1, 10), (1, 10), (2, 20)").unwrap();
    assert_eq!(answer("UPDATE t SET v = s.v + t.v FROM s WHERE t.id = s.id"), "2");
    assert_eq!(answer("SELECT * FROM t"), "1,10;2,20;3,0");
    assert_eq!(
        answer("UPDATE t AS x SET v = -1 FROM s AS y WHERE x.id = y.id AND y.v > 15 RETURNING *"),
        "2,-1"
    );
    assert_eq!(answer("DELETE FROM t USING s WHERE t.id = s.id RETURNING id"), "1;2");
    assert_eq!(answer("SELECT * FROM t"), "3,0");
    assert_eq!(answer("DELETE FROM t USING s"), "1");
    assert_eq!(answer("SELECT count(*) FROM t"), "0");
}

#[test]
fn a_set_of_several_columns_takes_a_row_one_value_each_or_one_value_for_all() {
    let db = Database::new();
    let answer = |sql: &str| {
        let result = db.execute(sql).unwrap();
        (0..result.len())
            .map(|row| {
                (0..result.width()).map(|at| result.text_at(row, at)).collect::<Vec<_>>().join(",")
            })
            .collect::<Vec<_>>()
            .join(";")
    };
    db.execute("CREATE TABLE t (k INTEGER, f VARCHAR, c INTEGER)").unwrap();
    db.execute("INSERT INTO t VALUES (1, 'apple', 2), (2, 'orange', 3)").unwrap();
    db.execute("UPDATE t SET (k, f, c) = (1, 'pear', 2)").unwrap();
    assert_eq!(answer("SELECT * FROM t"), "1,pear,2;1,pear,2");
    db.execute("UPDATE t SET (k, f, c) = ROW(2, 'fig', 3)").unwrap();
    assert_eq!(answer("SELECT * FROM t"), "2,fig,3;2,fig,3");
    db.execute("UPDATE t SET (k, f, c) = k + c").unwrap();
    assert_eq!(answer("SELECT * FROM t"), "5,5,5;5,5,5");
    for (sql, message) in [
        ("UPDATE t SET (k, f, c) = (1, 2)", "expected 3 values, got 2"),
        (
            "UPDATE t SET (k, f) = ()",
            "Parser Error: Could not perform assignment, expected 2 values, got 0",
        ),
    ] {
        let error = db.execute(sql).unwrap_err().to_string();
        assert!(error.contains(message), "{sql}: {error}");
    }
}

#[test]
fn a_with_ahead_of_a_write_is_in_scope_for_all_of_it() {
    let db = Database::new();
    let answer = |sql: &str| {
        let result = db.execute(sql).unwrap();
        (0..result.len())
            .map(|row| {
                (0..result.width()).map(|at| result.text_at(row, at)).collect::<Vec<_>>().join(",")
            })
            .collect::<Vec<_>>()
            .join(";")
    };
    db.execute("CREATE TABLE t (a INTEGER, b VARCHAR)").unwrap();
    assert_eq!(
        answer("WITH v AS (SELECT 5 AS a, 'cte' AS b) INSERT INTO t SELECT * FROM v RETURNING a"),
        "5"
    );
    db.execute("INSERT INTO t VALUES (3, 'x'), (10, 'y')").unwrap();
    assert_eq!(
        answer(
            "WITH n AS MATERIALIZED (SELECT 100 AS new_a, 3 AS old_a) UPDATE t SET a = n.new_a \
             FROM n WHERE t.a = n.old_a RETURNING a"
        ),
        "100"
    );
    assert_eq!(
        answer("WITH d AS (SELECT 10 AS x) DELETE FROM t WHERE a IN (SELECT x FROM d) RETURNING b"),
        "y"
    );
    assert_eq!(answer("SELECT * FROM t ORDER BY a"), "5,cte;100,x");
}

#[test]
fn a_column_default_fills_what_an_insert_leaves_out_the_way_the_pin_does() {
    let db = scripted(&[
        "CREATE TABLE t (i INTEGER DEFAULT 1+2, s VARCHAR DEFAULT 'x', k INT, q VARCHAR DEFAULT 'it''s')",
        "INSERT INTO t (k) VALUES (5)",
        "INSERT INTO t VALUES (DEFAULT, DEFAULT, 1, DEFAULT), (7, 'y', DEFAULT, 'z')",
        "INSERT INTO t DEFAULT VALUES",
    ]);
    let all = db
        .query("SELECT i, s, k, q FROM t")
        .unwrap()
        .rows()
        .map(|row| row.iter().map(|v| v.to_string()).collect::<Vec<_>>().join(","))
        .collect::<Vec<_>>();
    assert_eq!(all, ["3,x,5,it's", "3,x,1,it's", "7,y,NULL,z", "3,x,NULL,it's"]);
    let described = db
        .query("SELECT column_name, \"default\" FROM (DESCRIBE t)")
        .unwrap()
        .rows()
        .map(|row| format!("{}={}", row[0], row[1]))
        .collect::<Vec<_>>();
    assert_eq!(described, ["i=(1 + 2)", "s='x'", "k=NULL", "q='it''s'"]);
    for (statement, message) in [
        ("CREATE TABLE u (a INT DEFAULT k)", "DEFAULT value cannot contain column names"),
        ("CREATE TABLE u (a INT DEFAULT (SELECT 1))", "DEFAULT value cannot contain subqueries"),
        ("CREATE TABLE u (a INT DEFAULT sum(1))", "DEFAULT value cannot contain aggregates!"),
        (
            "CREATE TABLE u (a INT DEFAULT row_number() OVER ())",
            "DEFAULT value cannot contain window functions!",
        ),
        ("INSERT INTO t VALUES (DEFAULT + 1, 'a', 1, 'b')", "DEFAULT is not allowed here!"),
        (
            "INSERT INTO t (k) DEFAULT VALUES",
            "You can not provide both a column list and DEFAULT VALUES, please remove one of the \
             two",
        ),
    ] {
        assert_eq!(refusal(&db, statement), message, "{statement}");
    }
    db.execute("UPDATE t SET i = DEFAULT, k = DEFAULT, s = 'w' WHERE i = 7").unwrap();
    let updated = db.query("SELECT i, s, k FROM t WHERE s = 'w'").unwrap();
    let row: Vec<String> = updated.rows().next().unwrap().iter().map(|v| v.to_string()).collect();
    assert_eq!(row, ["3", "w", "NULL"]);
}

#[test]
fn a_check_constraint_refuses_a_row_that_fails_it_the_way_the_pin_does() {
    let db = scripted(&[
        "CREATE TABLE c (i INT CHECK (i > 0), j INT, CHECK (j < i))",
        "INSERT INTO c VALUES (1, 0)",
        "INSERT INTO c VALUES (NULL, NULL)",
        "CREATE TABLE e (i INT CHECK (i > 0) CHECK (i < 10), s VARCHAR CHECK (length(s) < 3))",
    ]);
    let failed = |table: &str, text: &str| {
        format!("CHECK constraint failed on table \"{table}\" with expression CHECK({text})")
    };
    for (statement, message) in [
        ("INSERT INTO c VALUES (0, -1)", failed("c", "(i > 0)")),
        ("INSERT INTO c VALUES (5, 6)", failed("c", "(j < i)")),
        ("UPDATE c SET i = -1 WHERE i = 1", failed("c", "(i > 0)")),
        ("UPDATE c SET j = 10", failed("c", "(j < i)")),
        ("INSERT INTO e VALUES (11, 'a')", failed("e", "(i < 10)")),
        ("INSERT INTO e VALUES (1, 'abcd')", failed("e", "(length(s) < 3)")),
        (
            "CREATE TABLE d (i INT CHECK (i > (SELECT 1)))",
            "subqueries prohibited in CHECK constraints".into(),
        ),
        (
            "CREATE TABLE d (i INT CHECK (sum(i) > 1))",
            "aggregate functions are not allowed in check constraints".into(),
        ),
        (
            "CREATE TABLE d (i INT CHECK (k > 1))",
            "Table does not contain column \"k\" referenced in check constraint!".into(),
        ),
    ] {
        assert_eq!(refusal(&db, statement), message, "{statement}");
    }
    db.execute("INSERT INTO e VALUES (1, 'ab')").unwrap();
    let all = db
        .query("SELECT i, j FROM c")
        .unwrap()
        .rows()
        .map(|row| format!("{},{}", row[0], row[1]))
        .collect::<Vec<_>>();
    assert_eq!(all, ["1,0", "NULL,NULL"]);
}

#[test]
fn a_foreign_key_holds_from_both_ends_the_way_the_pin_holds_it() {
    let db = scripted(&[
        "CREATE TABLE pkt (i INT PRIMARY KEY, j INT UNIQUE, k INT)",
        "CREATE TABLE fkt (a INT REFERENCES pkt, b INT, FOREIGN KEY (b) REFERENCES pkt (j))",
        "INSERT INTO pkt VALUES (1, 10, 100), (2, 20, 200), (3, 30, 300)",
        "INSERT INTO fkt VALUES (1, 20), (NULL, NULL)",
        "UPDATE pkt SET k = 5 WHERE i = 1",
        "DELETE FROM pkt WHERE i = 3",
        "CREATE VIEW pkv AS SELECT * FROM pkt",
    ]);
    let missing = |key: &str| {
        format!(
            "Violates foreign key constraint because key \"{key}\" does not exist in the \
             referenced table"
        )
    };
    let held = |key: &str| {
        format!(
            "Violates foreign key constraint because key \"{key}\" is still referenced by a \
             foreign key in a different table. If this is an unexpected constraint violation, \
             please refer to our foreign key limitations in the documentation"
        )
    };
    for (statement, message) in [
        ("INSERT INTO fkt VALUES (4, 10)", missing("i: 4")),
        ("INSERT INTO fkt VALUES (1, 30)", missing("j: 30")),
        ("UPDATE fkt SET a = 3 WHERE a = 1", missing("i: 3")),
        ("UPDATE pkt SET i = 7 WHERE i = 1", held("a: 1")),
        ("UPDATE pkt SET j = 21 WHERE i = 2", held("b: 20")),
        ("DELETE FROM pkt", held("a: 1")),
        (
            "DROP TABLE pkt",
            "Could not drop the table because this table is main key table of the table \"fkt\""
                .into(),
        ),
        (
            "CREATE TABLE f2 (a INT REFERENCES pkt (k))",
            "Failed to create foreign key: referenced table \"pkt\" does not have a primary key or \
             unique constraint on the columns k"
                .into(),
        ),
        (
            "CREATE TABLE f2 (a VARCHAR REFERENCES pkt)",
            "Failed to create foreign key: incompatible types between column \"i\" (\"INTEGER\") \
             and column \"a\" (\"VARCHAR\")"
                .into(),
        ),
        ("CREATE TABLE f2 (a INT REFERENCES pkv (i))", "cannot reference a VIEW with a FOREIGN KEY".into()),
        (
            "CREATE TABLE f2 (a INT REFERENCES fkt)",
            "Failed to create foreign key: there is no primary key for referenced table \"fkt\""
                .into(),
        ),
    ] {
        assert_eq!(refusal(&db, statement), message, "{statement}");
    }
    db.execute("DELETE FROM fkt").unwrap();
    db.execute("DELETE FROM pkt WHERE i = 1").unwrap();
    db.execute("DROP TABLE fkt").unwrap();
    db.execute("DROP TABLE pkt").unwrap();
    let db = scripted(&["CREATE TABLE s (i INT PRIMARY KEY, p INT REFERENCES s (i))"]);
    assert_eq!(refusal(&db, "INSERT INTO s VALUES (1, 1)"), missing("i: 1"));
    db.execute("INSERT INTO s VALUES (1, NULL)").unwrap();
    db.execute("INSERT INTO s VALUES (2, 1)").unwrap();
}

#[test]
fn an_insert_that_meets_a_held_key_does_what_its_conflict_clause_says() {
    let db = scripted(&[
        "CREATE TABLE t (i INTEGER PRIMARY KEY, j INTEGER, k INTEGER)",
        "INSERT INTO t VALUES (1, 1, 1), (2, 2, 2)",
    ]);
    let all = |db: &Database| {
        db.query("SELECT i, j, k FROM t ORDER BY i")
            .unwrap()
            .rows()
            .map(|row| row.iter().map(|v| v.to_string()).collect::<Vec<_>>().join(","))
            .collect::<Vec<_>>()
            .join(" ")
    };
    // The first row with a key is the one that counts, held or not.
    let count = db
        .execute("INSERT INTO t VALUES (1, 6, 6), (1, 5, 5), (3, 3, 3), (3, 4, 4) ON CONFLICT DO NOTHING")
        .unwrap();
    assert_eq!(count.rows().next().unwrap()[0], Value::BigInt(1));
    assert_eq!(all(&db), "1,1,1 2,2,2 3,3,3");
    db.execute(
        "INSERT INTO t VALUES (1, 6, 6), (1, 5, 5) ON CONFLICT (i) DO UPDATE SET j = excluded.j",
    )
    .unwrap();
    assert_eq!(all(&db), "1,6,1 2,2,2 3,3,3");
    // An unqualified name is the held row, the insert alias names it too, and the condition picks
    // which rows are updated and counted.
    let count = db
        .execute(
            "INSERT INTO t AS x VALUES (1, 0, 10), (2, 0, 20), (4, 4, 4) ON CONFLICT DO UPDATE \
             SET k = x.k + excluded.k WHERE j > 5",
        )
        .unwrap();
    assert_eq!(count.rows().next().unwrap()[0], Value::BigInt(2));
    assert_eq!(all(&db), "1,6,11 2,2,2 3,3,3 4,4,4");
    // Replace takes only the columns the insert names.
    db.execute("INSERT OR REPLACE INTO t (i, j) VALUES (2, 9)").unwrap();
    db.execute("INSERT OR IGNORE INTO t VALUES (3, 0, 0)").unwrap();
    assert_eq!(all(&db), "1,6,11 2,9,2 3,3,3 4,4,4");
    // A key column can be set, and a set that repeats a key is refused.
    db.execute("INSERT INTO t VALUES (4, 0, 0) ON CONFLICT DO UPDATE SET i = 7").unwrap();
    assert_eq!(all(&db), "1,6,11 2,9,2 3,3,3 7,4,4");
    assert_eq!(
        refusal(&db, "INSERT INTO t VALUES (7, 0, 0) ON CONFLICT DO UPDATE SET i = 1"),
        "Duplicate key \"i: 1\" violates primary key constraint."
    );
    let returned = db
        .execute("INSERT INTO t VALUES (8, 8, 8), (1, 0, 0) ON CONFLICT DO UPDATE SET j = 0 RETURNING i, j")
        .unwrap()
        .rows()
        .map(|row| format!("{}:{}", row[0], row[1]))
        .collect::<Vec<_>>();
    assert_eq!(returned, ["1:0", "8:8"]);
}

#[test]
fn a_conflict_clause_that_names_no_key_is_refused_the_way_the_pin_refuses_it() {
    let db = scripted(&[
        "CREATE TABLE plain (i INTEGER)",
        "CREATE TABLE t (i INTEGER PRIMARY KEY, j INTEGER UNIQUE)",
        "INSERT INTO t VALUES (1, 1)",
    ]);
    for (statement, message) in [
        (
            "INSERT OR IGNORE INTO plain VALUES (1)",
            "There are no UNIQUE/PRIMARY KEY constraints that refer to this table, specify ON \
             CONFLICT columns manually",
        ),
        (
            "INSERT INTO plain VALUES (1) ON CONFLICT (i) DO NOTHING",
            "The specified columns as conflict target are not referenced by a UNIQUE/PRIMARY KEY \
             CONSTRAINT or INDEX",
        ),
        (
            "INSERT INTO t VALUES (1, 1) ON CONFLICT DO UPDATE SET j = 2",
            "Conflict target has to be provided for a DO UPDATE operation when the table has \
             multiple UNIQUE/PRIMARY KEY constraints",
        ),
        (
            "INSERT INTO t VALUES (1, 1) ON CONFLICT (zz) DO NOTHING",
            "Table \"t\" does not have a column with name \"zz\"",
        ),
        (
            "INSERT INTO t VALUES (1, 1) ON CONFLICT (i) DO UPDATE SET zz = 1",
            "Referenced update column zz not found in table!",
        ),
        (
            "INSERT INTO t VALUES (1, 1) ON CONFLICT (i) DO UPDATE SET j = 1, j = 2",
            "Multiple assignments to same column \"\"j\"\"",
        ),
        (
            "INSERT INTO t VALUES (2, 1) ON CONFLICT (i) DO NOTHING",
            "Duplicate key \"j: 1\" violates unique constraint.",
        ),
    ] {
        assert_eq!(refusal(&db, statement), message, "{statement}");
    }
    db.execute("INSERT INTO t VALUES (2, 1) ON CONFLICT DO NOTHING").unwrap();
    db.execute("INSERT INTO t VALUES (1, 5) ON CONFLICT (i) DO UPDATE SET j = excluded.j").unwrap();
    assert_eq!(db.query("SELECT j FROM t").unwrap().rows().next().unwrap()[0], Value::Integer(5));
}

#[test]
fn a_key_refuses_a_write_that_would_repeat_it_the_way_the_pin_does() {
    let db = scripted(&[
        "CREATE TABLE t (i INTEGER PRIMARY KEY, s VARCHAR UNIQUE)",
        "INSERT INTO t VALUES (1, 'a'), (2, NULL), (3, NULL)",
    ]);
    for (statement, message) in [
        (
            "INSERT INTO t VALUES (4, 'b'), (4, 'c')",
            "PRIMARY KEY or UNIQUE constraint violation: duplicate key \"4\"",
        ),
        (
            "INSERT INTO t VALUES (5, 'x'), (1, 'y')",
            "Duplicate key \"i: 1\" violates primary key constraint.",
        ),
        ("INSERT INTO t VALUES (6, 'a')", "Duplicate key \"s: a\" violates unique constraint."),
        ("INSERT INTO t VALUES (NULL, 'z')", "NOT NULL constraint failed: t.i"),
        (
            "UPDATE t SET i = 2 WHERE i = 1",
            "Duplicate key \"i: 2\" violates primary key constraint.",
        ),
        (
            "CREATE TABLE u (i INTEGER, PRIMARY KEY (k))",
            "table \"u\" does not have a column named \"k\"",
        ),
    ] {
        assert_eq!(refusal(&db, statement), message, "{statement}");
    }
    // Nothing a refused write touched is left behind, and a shift of every key is fine because it
    // is the rows once the statement is done that are checked.
    db.execute("UPDATE t SET i = i + 1").unwrap();
    db.execute("INSERT INTO t VALUES (1, 'b'), (5, NULL)").unwrap();
    assert_eq!(
        db.query("SELECT i, s FROM t ORDER BY i").unwrap().rows().collect::<Vec<_>>(),
        vec![
            vec![Value::Integer(1), Value::Varchar("b".into())],
            vec![Value::Integer(2), Value::Varchar("a".into())],
            vec![Value::Integer(3), Value::Null],
            vec![Value::Integer(4), Value::Null],
            vec![Value::Integer(5), Value::Null],
        ]
    );
    // A rolled back insert takes its keys with it.
    db.execute("BEGIN").unwrap();
    db.execute("INSERT INTO t VALUES (9, 'q')").unwrap();
    db.execute("ROLLBACK").unwrap();
    db.execute("INSERT INTO t VALUES (9, 'q')").unwrap();
    db.execute("CREATE TABLE u (a INTEGER, b INTEGER, c INTEGER, UNIQUE (a, b), PRIMARY KEY (b))")
        .unwrap();
    let described: Vec<Vec<Value>> =
        db.query("SELECT \"null\", \"key\" FROM (DESCRIBE u)").unwrap().rows().collect();
    let text = |value: &str| Value::Varchar(value.into());
    assert_eq!(
        described,
        vec![
            vec![text("YES"), text("UNI")],
            vec![text("NO"), text("PRI")],
            vec![text("YES"), Value::Null],
        ]
    );
    // Through a query as well, for a column passed straight through and for nothing computed.
    let described: Vec<Vec<Value>> = db
        .query("SELECT \"key\" FROM (DESCRIBE SELECT c, b AS x, b + 0 FROM u WHERE a > 1)")
        .unwrap()
        .rows()
        .collect();
    assert_eq!(described, vec![vec![Value::Null], vec![text("PRI")], vec![Value::Null]]);
}

#[test]
fn a_schema_is_made_filled_and_dropped_the_way_the_pin_does_it() {
    let db = scripted(&[
        "CREATE SCHEMA s1",
        "CREATE SCHEMA IF NOT EXISTS s1",
        "CREATE TABLE s1.t (i INT)",
        "CREATE VIEW s1.v AS SELECT 1 AS one",
        "INSERT INTO s1.t VALUES (1), (2)",
        "CREATE SCHEMA empty",
        "CREATE OR REPLACE SCHEMA empty",
        "DROP SCHEMA IF EXISTS gone",
    ]);
    let count =
        db.query("SELECT count(*) FROM s1.t").unwrap().rows().next().unwrap()[0].to_string();
    assert_eq!(count, "2");
    for (statement, message) in [
        ("CREATE SCHEMA s1", "Schema with name \"s1\" already exists!"),
        ("CREATE SCHEMA nodb.s3", "\"nodb\" is not a catalog or schema"),
        ("CREATE SCHEMA memory.a.b", "\"a\" is not a catalog or schema"),
        ("CREATE TEMP SCHEMA s4", "Temporary schemas are not supported"),
        ("CREATE SCHEMA pg_catalog", "Cannot create schema in system catalog"),
        ("CREATE SCHEMA system.x", "Cannot create schema in system catalog"),
        ("CREATE SCHEMA temp.x", "Cannot create non-temporary entry \"x\" in temporary catalog"),
        ("DROP SCHEMA gone", "Schema with name gone does not exist!"),
        ("DROP SCHEMA main", "Cannot drop entry \"main\" because it is an internal system entry"),
        (
            "DROP SCHEMA system.pg_catalog",
            "Cannot drop entry \"pg_catalog\" because it is an internal system entry",
        ),
        ("DROP SCHEMA s1, empty", "Can only drop one object at a time"),
        (
            "SELECT * FROM s2.t",
            "Table with name \"s2.t\" does not exist because schema \"s2\" does not exist.",
        ),
        (
            "DROP SCHEMA s1",
            "Cannot drop entry \"s1\" because there are entries that depend on it.\nview \"v\" \
             depends on schema \"s1\".\ntable \"t\" depends on schema \"s1\".\nUse DROP...CASCADE \
             to drop all dependents.",
        ),
    ] {
        assert_eq!(refusal(&db, statement), message, "{statement}");
    }
    let error = db.execute("CREATE OR REPLACE SCHEMA s1").unwrap_err();
    assert_eq!(error.code(), rudb_common::ErrorCode::Dependency);
    db.execute("BEGIN").unwrap();
    db.execute("DROP SCHEMA s1 CASCADE").unwrap();
    db.execute("ROLLBACK").unwrap();
    db.execute("SELECT * FROM s1.v").unwrap();
    db.execute("DROP SCHEMA s1 CASCADE").unwrap();
    db.execute("DROP SCHEMA empty").unwrap();
    assert!(refusal(&db, "SELECT * FROM s1.t").contains("because schema \"s1\" does not exist"));
}

#[test]
fn a_sequence_counts_the_way_the_pin_counts() {
    let db = scripted(&[
        "CREATE SEQUENCE seq",
        "CREATE SEQUENCE down INCREMENT BY -2 MINVALUE -5 MAXVALUE 5 CYCLE",
        "CREATE TABLE u (id BIGINT DEFAULT nextval('seq'), v INT)",
    ]);
    let values = |sql: &str| -> Vec<String> {
        db.query(sql).unwrap().rows().map(|row| row[0].to_string()).collect()
    };
    assert_eq!(values("SELECT nextval('seq') FROM range(3)"), ["1", "2", "3"]);
    assert_eq!(values("SELECT currval('seq')"), ["3"]);
    assert_eq!(values("SELECT setval('seq', 20, false)"), ["20"]);
    assert_eq!(values("SELECT nextval('seq')"), ["20"]);
    assert_eq!(
        values("SELECT nextval('down') FROM range(8)"),
        ["5", "3", "1", "-1", "-3", "-5", "5", "3"]
    );
    db.execute("INSERT INTO u (v) VALUES (1), (2)").unwrap();
    assert_eq!(values("SELECT id FROM u"), ["21", "22"]);
    // A value handed out inside a transaction that rolls back stays handed out.
    db.execute("BEGIN").unwrap();
    assert_eq!(values("SELECT nextval('seq')"), ["23"]);
    db.execute("ROLLBACK").unwrap();
    assert_eq!(values("SELECT nextval('seq')"), ["24"]);
    assert_eq!(values("SELECT nextval(NULL)"), ["NULL"]);
    for (statement, message) in [
        ("CREATE SEQUENCE seq", "Sequence with name \"seq\" already exists!"),
        ("CREATE SEQUENCE bad INCREMENT 0", "Increment must not be zero"),
        ("CREATE SEQUENCE bad START 0", "START value (0) cannot be less than MINVALUE (1)"),
        ("CREATE SEQUENCE bad CYCLE CYCLE", "Cycle should be passed at most once"),
        ("CREATE SEQUENCE bad INCREMENT BY 1+1", "Expected a minus function instead of \"+\""),
        ("SELECT nextval('nope')", "Sequence with name nope does not exist!"),
        (
            "SELECT setval('seq', 0)",
            "setval: value 0 is out of bounds for sequence \"seq\" (1..9223372036854775807)",
        ),
        ("DROP SEQUENCE seq, down", "Can only drop one object at a time"),
        ("SELECT setval('seq', 'abc')", "Could not convert string 'abc' to INT64"),
        ("SELECT setval('seq', 5, 'maybe')", "Could not convert string 'maybe' to BOOL"),
        (
            "CREATE SEQUENCE bad INCREMENT 1 START -1",
            "START value (-1) cannot be less than MINVALUE (1)",
        ),
        (
            "SELECT nextval('a.b.c.d')",
            "Sequence with name \"a.b.c.d\" does not exist because schema \"a.b.c\" does not \
             exist.",
        ),
        (
            "DROP SEQUENCE seq",
            "Cannot drop entry \"seq\" because there are entries that depend on it.\ntable \"u\" \
             depends on sequence \"seq\".\nUse DROP...CASCADE to drop all dependents.",
        ),
    ] {
        assert_eq!(refusal(&db, statement), message, "{statement}");
    }
    assert!(refusal(&db, "SELECT setval('seq', 5.5)").starts_with("No function matches"));
    assert!(refusal(&db, "SELECT setval('seq', 1, true, true)").starts_with("No function matches"));
    db.execute("DROP SEQUENCE seq CASCADE").unwrap();
    assert!(refusal(&db, "SELECT * FROM u").contains("Table with name u does not exist"));
    db.execute("DROP SEQUENCE IF EXISTS seq").unwrap();
}

#[test]
fn duckdb_sequences_lists_each_sequence_where_it_has_got_to() {
    let db = scripted(&[
        "CREATE SEQUENCE s INCREMENT 2 START 5 MAXVALUE 100 CYCLE",
        "SELECT nextval('s')",
        "CREATE TEMP SEQUENCE t",
    ]);
    let rows: Vec<Vec<String>> = db
        .query(
            "SELECT database_name, sequence_name, temporary, start_value, max_value, cycle, \
             last_value, sql FROM duckdb_sequences() ORDER BY sequence_name",
        )
        .unwrap()
        .rows()
        .map(|row| row.iter().map(ToString::to_string).collect())
        .collect();
    assert_eq!(
        rows,
        [
            [
                "memory",
                "s",
                "false",
                "5",
                "100",
                "true",
                "5",
                "CREATE SEQUENCE s INCREMENT BY 2 MINVALUE 1 MAXVALUE 100 START 7 CYCLE;",
            ],
            [
                "temp",
                "t",
                "true",
                "1",
                "9223372036854775807",
                "false",
                "NULL",
                "CREATE SEQUENCE t INCREMENT BY 1 MINVALUE 1 MAXVALUE 9223372036854775807 START 1 NO \
                 CYCLE;",
            ],
        ]
    );
}

#[test]
fn a_sequence_owned_by_a_table_goes_with_it() {
    let db = scripted(&[
        "CREATE SEQUENCE s",
        "CREATE SEQUENCE other",
        "CREATE TABLE t (i INTEGER)",
        "CREATE TABLE u (i INTEGER)",
        "ALTER SEQUENCE s OWNED BY t",
        "ALTER SEQUENCE s OWNED BY t",
        "ALTER SEQUENCE IF EXISTS gone OWNED BY nothing",
    ]);
    for (statement, message) in [
        ("ALTER SEQUENCE s OWNED BY u", "\"s\" is already owned by \"t\""),
        ("ALTER SEQUENCE s OWNED BY t OWNED BY u", "Owned by value should be passed at most once"),
        ("ALTER SEQUENCE other OWNED BY nope", "CatalogElement \"main.nope\" does not exist!"),
        ("ALTER SEQUENCE gone OWNED BY t", "Sequence with name gone does not exist!"),
        ("ALTER SEQUENCE s INCREMENT 2", "ALTER SEQUENCE option not yet supported"),
        (
            "DROP SEQUENCE s",
            "Cannot drop entry \"s\" because there are entries that depend on it.\ntable \"t\" \
             depends on sequence \"s\".\nUse DROP...CASCADE to drop all dependents.",
        ),
    ] {
        assert_eq!(refusal(&db, statement), message, "{statement}");
    }
    db.execute("DROP TABLE t").unwrap();
    assert!(refusal(&db, "SELECT nextval('s')").contains("Sequence with name s does not exist!"));
    db.execute("ALTER SEQUENCE other OWNED BY u").unwrap();
    db.execute("DROP SEQUENCE other CASCADE").unwrap();
    assert!(refusal(&db, "SELECT * FROM u").contains("Table with name u does not exist"));
}

#[test]
fn alter_table_changes_a_table_the_way_the_pin_does() {
    let db = scripted(&[
        "CREATE TABLE u (a INT PRIMARY KEY, b INT CHECK (b > 0), c VARCHAR)",
        "INSERT INTO u VALUES (1, 10, 'x'), (2, 20, NULL)",
    ]);
    let values = |sql: &str| -> Vec<String> {
        db.query(sql)
            .unwrap()
            .rows()
            .map(|row| row.iter().map(|v| v.to_string()).collect::<Vec<_>>().join("|"))
            .collect()
    };
    db.execute("ALTER TABLE u ADD COLUMN d INT DEFAULT 7").unwrap();
    db.execute("ALTER TABLE u ADD COLUMN IF NOT EXISTS d INT").unwrap();
    db.execute("ALTER TABLE u ADD e DOUBLE").unwrap();
    assert_eq!(values("SELECT a, d, e FROM u ORDER BY a"), ["1|7|NULL", "2|7|NULL"]);
    db.execute("INSERT INTO u (a, b) VALUES (3, 30)").unwrap();
    assert_eq!(values("SELECT d FROM u WHERE a = 3"), ["7"]);
    db.execute("ALTER TABLE u RENAME COLUMN b TO bb").unwrap();
    assert!(
        refusal(&db, "INSERT INTO u (a, bb) VALUES (4, -1)").contains("CHECK constraint failed")
    );
    db.execute("ALTER TABLE u DROP COLUMN e").unwrap();
    db.execute("ALTER TABLE u DROP COLUMN IF EXISTS zz").unwrap();
    db.execute("ALTER TABLE u DROP COLUMN bb").unwrap();
    db.execute("INSERT INTO u (a) VALUES (4)").unwrap();
    assert_eq!(values("SELECT a FROM u ORDER BY a"), ["1", "2", "3", "4"]);
    db.execute("ALTER TABLE u ALTER COLUMN d TYPE VARCHAR").unwrap();
    db.execute("ALTER TABLE u ALTER d SET DEFAULT 'q'").unwrap();
    db.execute("INSERT INTO u (a) VALUES (5)").unwrap();
    assert_eq!(values("SELECT d FROM u WHERE a = 5"), ["q"]);
    db.execute("ALTER TABLE u ALTER COLUMN d TYPE INT USING length(d)").unwrap();
    assert_eq!(values("SELECT sum(d) FROM u"), ["5"]);
    db.execute("ALTER TABLE u ALTER d DROP DEFAULT").unwrap();
    db.execute("INSERT INTO u (a) VALUES (6)").unwrap();
    assert_eq!(values("SELECT d FROM u WHERE a = 6"), ["NULL"]);
    assert!(
        refusal(&db, "ALTER TABLE u ALTER d SET NOT NULL")
            .contains("NOT NULL constraint failed: u.d")
    );
    db.execute("ALTER TABLE u ALTER c DROP NOT NULL").unwrap();
    assert!(
        refusal(&db, "ALTER TABLE u ALTER a DROP NOT NULL")
            .contains("column \"a\" is in a primary key")
    );
    db.execute("ALTER TABLE u RENAME TO w").unwrap();
    db.execute("ALTER TABLE IF EXISTS u RENAME TO z").unwrap();
    assert_eq!(values("SELECT count(*) FROM w"), ["6"]);
    for (statement, message) in [
        ("ALTER TABLE u ADD COLUMN q INT", "Table with name u does not exist!"),
        ("ALTER TABLE w DROP COLUMN zz", "Table \"w\" does not have a column with name \"zz\""),
        ("ALTER TABLE w ADD COLUMN c INT", "Column with name \"c\" already exists!"),
        ("ALTER TABLE w RENAME COLUMN c TO d", "Column with name \"d\" already exists!"),
        ("ALTER TABLE w DROP COLUMN a", "because there is a UNIQUE constraint that depends on it"),
        ("ALTER TABLE w ALTER a TYPE BIGINT", "has a UNIQUE or PRIMARY KEY constraint specified"),
        (
            "ALTER TABLE w ADD COLUMN IF NOT EXISTS n INT NOT NULL",
            "with IF NOT EXISTS is not supported",
        ),
        ("ALTER TABLE w ADD COLUMN n INT NOT NULL", "NOT NULL constraint failed: w.n"),
        ("ALTER TABLE w DROP CONSTRAINT k", "No support for that ALTER TABLE option yet!"),
    ] {
        assert!(
            refusal(&db, statement).contains(message),
            "{statement}: {}",
            refusal(&db, statement)
        );
    }
    db.execute("CREATE TABLE one (x INT)").unwrap();
    assert!(
        refusal(&db, "ALTER TABLE one DROP COLUMN x").contains("only has one column remaining")
    );
    assert!(
        refusal(&db, "ALTER TABLE one RENAME TO w")
            .contains("another entry with this name already exists")
    );
    db.execute("CREATE VIEW v AS SELECT 1 AS x").unwrap();
    db.execute("ALTER VIEW v RENAME TO vv").unwrap();
    db.execute("ALTER TABLE vv RENAME TO v2").unwrap();
    assert!(
        refusal(&db, "ALTER TABLE v2 ADD COLUMN y INT")
            .contains("Can only modify view with ALTER VIEW statement")
    );
    assert!(
        refusal(&db, "ALTER VIEW w RENAME TO z")
            .contains("Can only modify table with ALTER TABLE statement")
    );
    assert_eq!(values("SELECT x FROM v2"), ["1"]);
}

#[test]
fn a_table_another_table_references_refuses_most_alters() {
    let db = scripted(&[
        "CREATE TABLE p (a INT PRIMARY KEY, b INT)",
        "CREATE TABLE f (x INT REFERENCES p(a), y INT)",
    ]);
    db.execute("ALTER TABLE p ADD COLUMN c INT").unwrap();
    db.execute("ALTER TABLE p ALTER b SET DEFAULT 3").unwrap();
    for statement in [
        "ALTER TABLE p RENAME TO q",
        "ALTER TABLE p DROP COLUMN b",
        "ALTER TABLE p ALTER b SET NOT NULL",
    ] {
        assert!(
            refusal(&db, statement)
                .contains("Cannot alter entry \"p\" because there are entries that depend on it."),
            "{statement}"
        );
    }
    for statement in ["ALTER TABLE p RENAME COLUMN a TO aa", "ALTER TABLE f RENAME COLUMN x TO xx"]
    {
        assert!(
            refusal(&db, statement)
                .contains("because this is involved in the foreign key constraint"),
            "{statement}"
        );
    }
    db.execute("ALTER TABLE f RENAME COLUMN y TO yy").unwrap();
    for (kind, statement) in [
        ("PRIMARY KEY", "ALTER TABLE f ADD COLUMN z INT PRIMARY KEY"),
        ("CHECK", "ALTER TABLE f ADD COLUMN z INT CHECK (z > 0)"),
        ("FOREIGN KEY", "ALTER TABLE f ADD COLUMN z INT REFERENCES p(a)"),
    ] {
        let message = format!("Adding columns with {kind} constraints is not supported yet");
        assert!(refusal(&db, statement).contains(&message), "{statement}");
    }
    let dropped = refusal(&db, "ALTER TABLE f DROP COLUMN x");
    assert!(dropped.contains("FOREIGN KEY constraint that depends on it"), "{dropped}");
}

#[test]
fn a_float_cast_to_a_whole_number_rounds_half_to_even_and_a_decimal_does_not() {
    let db = Database::new();
    let answer = rows(
        &db,
        "SELECT CAST(2.5::DOUBLE AS BIGINT), CAST(3.5::DOUBLE AS BIGINT), \
         CAST(-2.5::DOUBLE AS BIGINT), CAST(2.5 AS BIGINT), CAST(2.5::FLOAT AS INT)",
    );
    assert_eq!(
        answer,
        vec![vec![
            Value::BigInt(2),
            Value::BigInt(4),
            Value::BigInt(-2),
            Value::BigInt(3),
            Value::Integer(2)
        ]]
    );
}

#[test]
fn create_index_keeps_the_index_and_refuses_what_the_pin_refuses() {
    let db = scripted(&[
        "CREATE TABLE t (a INT, b INT, l INT[])",
        "INSERT INTO t VALUES (1, 1, NULL), (2, 1, NULL)",
    ]);
    let values = |sql: &str| -> Vec<String> {
        db.query(sql)
            .unwrap()
            .rows()
            .map(|row| row.iter().map(|v| v.to_string()).collect::<Vec<_>>().join("|"))
            .collect()
    };
    db.execute("CREATE INDEX i1 ON t(t.a)").unwrap();
    db.execute("CREATE INDEX i2 ON t USING art (a)").unwrap();
    db.execute("CREATE INDEX i3 ON t((a+b), b DESC)").unwrap();
    db.execute("CREATE INDEX IF NOT EXISTS i3 ON t(a)").unwrap();
    db.execute("CREATE INDEX t ON t(b)").unwrap();
    assert_eq!(
        values("SELECT index_name, expressions, sql FROM duckdb_indexes() ORDER BY index_oid"),
        [
            "i1|[a]|CREATE INDEX i1 ON t(a);",
            "i2|[a]|CREATE INDEX i2 ON t USING art (a);",
            "i3|['((a + b))', b]|CREATE INDEX i3 ON t(((a + b)), b);",
            "t|[b]|CREATE INDEX t ON t(b);",
        ]
    );
    assert_eq!(values("SELECT index_count FROM duckdb_tables()"), ["4"]);
    let refusals = [
        ("CREATE INDEX i1 ON t(b)", "Index with name \"i1\" already exists!"),
        ("CREATE OR REPLACE INDEX i1 ON t(b)", "Index with name \"i1\" already exists!"),
        ("CREATE INDEX i9 ON t(zz)", "Table \"t\" does not have a column named \"zz\""),
        ("CREATE INDEX i9 ON t(a) WHERE a > 1", "partial indexes is not supported"),
        ("CREATE INDEX i9 ON t((SELECT 1))", "cannot use subquery in index expressions"),
        ("CREATE INDEX i9 ON t(sum(a))", "aggregate functions are not allowed in index"),
        ("CREATE INDEX i9 ON t(l)", "Invalid Type [INTEGER[]]: Invalid type for index key."),
        ("CREATE INDEX i9 ON t(1)", "does not refer to any columns in the base table!"),
        ("CREATE INDEX i9 ON t USING hash (a)", "Unknown index type: hash"),
        ("CREATE TEMP INDEX i9 ON t(a)", "Temporary indexes are not supported"),
        ("CREATE INDEX ON t(a)", "Please provide an index name"),
        ("CREATE INDEX i9 ON t(a COLLATE nocase)", "Index with collation not supported yet!"),
        ("CREATE UNIQUE INDEX u1 ON t(b)", "Data contains duplicates on indexed column(s)"),
        ("DROP INDEX i1, i2", "Can only drop one object at a time"),
        ("DROP INDEX nope", "Index with name nope does not exist!"),
    ];
    for (sql, wanted) in refusals {
        let got = refusal(&db, sql);
        assert!(got.contains(wanted), "{sql}: {got}");
    }
    db.execute("CREATE UNIQUE INDEX u1 ON t(a)").unwrap();
    assert!(refusal(&db, "INSERT INTO t VALUES (1, 5, NULL)").contains("Duplicate key \"a: 1\""));
    db.execute("INSERT INTO t VALUES (NULL, 5, NULL), (NULL, 6, NULL)").unwrap();
    let alters = [
        ("ALTER TABLE t ALTER a TYPE BIGINT", "an index depends on it!"),
        ("ALTER TABLE t DROP COLUMN a", "an index depends on a column after it!"),
        ("ALTER TABLE t RENAME TO t2", "there are entries that depend on it."),
    ];
    for (sql, wanted) in alters {
        let got = refusal(&db, sql);
        assert!(got.contains(wanted), "{sql}: {got}");
    }
    db.execute("ALTER TABLE t ALTER b SET DEFAULT 3").unwrap();
    db.execute("DROP INDEX u1").unwrap();
    db.execute("DROP INDEX IF EXISTS u1").unwrap();
    db.execute("INSERT INTO t (a) VALUES (1)").unwrap();
    db.execute("BEGIN").unwrap();
    db.execute("CREATE INDEX r1 ON t(a)").unwrap();
    db.execute("ROLLBACK").unwrap();
    assert_eq!(values("SELECT count(*) FROM duckdb_indexes() WHERE index_name = 'r1'"), ["0"]);
}

#[test]
fn duckdb_constraints_lists_them_the_way_the_pin_does() {
    let db = scripted(&[
        "CREATE TABLE p (a INT PRIMARY KEY, b INT NOT NULL UNIQUE, c INT CHECK (c > 0))",
        "CREATE TABLE q (z INT REFERENCES p(a), x INT, y INT, CHECK (x + y > abs(x)))",
    ]);
    let values = |sql: &str| -> Vec<String> {
        db.query(sql)
            .unwrap()
            .rows()
            .map(|row| row.iter().map(|v| v.to_string()).collect::<Vec<_>>().join("|"))
            .collect()
    };
    let sql = "SELECT table_name, constraint_index, constraint_type, constraint_text, expression, \
               constraint_column_indexes, constraint_column_names, constraint_name, \
               referenced_table, referenced_column_names FROM duckdb_constraints()";
    assert_eq!(
        values(sql),
        [
            "p|0|PRIMARY KEY|PRIMARY KEY(a)|NULL|[0]|[a]|p_a_pkey|NULL|[]",
            "p|1|NOT NULL|NOT NULL|NULL|[1]|[b]|p_b_not_null|NULL|[]",
            "p|2|UNIQUE|UNIQUE(b)|NULL|[1]|[b]|p_b_key|NULL|[]",
            "p|3|CHECK|CHECK((c > 0))|(c > 0)|[2]|[c]|p_c_check|NULL|[]",
            "p|4|NOT NULL|NOT NULL|NULL|[0]|[a]|p_a_not_null|NULL|[]",
            "q|5|FOREIGN KEY|FOREIGN KEY (z) REFERENCES p(a)|NULL|[0]|[z]|q_z_a_fkey|p|[a]",
            "q|6|CHECK|CHECK(((x + y) > abs(x)))|((x + y) > abs(x))|[1, 2, 1]|[x, y, x]|\
             q_x_y_x_check|NULL|[]",
        ]
    );
    db.execute("ALTER TABLE q ADD COLUMN v INT").unwrap();
    db.execute("ALTER TABLE q ADD COLUMN w INT").unwrap();
    db.execute("ALTER TABLE q ALTER w SET NOT NULL").unwrap();
    db.execute("ALTER TABLE q DROP COLUMN v").unwrap();
    assert_eq!(
        values(
            "SELECT constraint_type, constraint_column_names FROM duckdb_constraints() \
                WHERE table_name = 'q'"
        ),
        ["FOREIGN KEY|[z]", "CHECK|[x, y, x]", "NOT NULL|[w]"]
    );
}

/// `SET schema`, `SET search_path` and `USE` move where a bare name is created and looked for, and
/// read back the way the pin prints them.
#[test]
fn set_schema_and_search_path_follow_the_pin() {
    let db = scripted(&["CREATE SCHEMA s1", "CREATE SCHEMA s2"]);
    let values = |sql: &str| -> Vec<String> {
        db.query(sql)
            .unwrap()
            .rows()
            .map(|row| row.iter().map(|v| v.to_string()).collect::<Vec<_>>().join("|"))
            .collect()
    };
    let path = "SELECT current_schema(), current_setting('search_path'), current_schemas(false)";
    let error = db.execute("SET schema = 'nope'").unwrap_err().to_string();
    assert!(error.contains("SET schema: No catalog + schema named \"nope\" found."), "{error}");
    db.execute("SET schema = s1").unwrap();
    db.execute("CREATE TABLE t1(a INT)").unwrap();
    db.execute("CREATE TABLE p(id INT PRIMARY KEY)").unwrap();
    db.execute("CREATE TABLE f(id INT REFERENCES p(id))").unwrap();
    assert_eq!(values(path), ["s1|s1|[s1]"]);
    db.execute("SET search_path = 's2,s1'").unwrap();
    db.execute("CREATE TABLE t2(a INT)").unwrap();
    assert_eq!(values(path), ["s2|s2,s1|[s2, s1]"]);
    assert_eq!(values("SELECT count(*) FROM t1"), ["0"]);
    assert_eq!(
        values("SELECT schema_name, table_name FROM duckdb_tables() ORDER BY ALL"),
        ["s1|f", "s1|p", "s1|t1", "s2|t2"]
    );
    assert_eq!(
        values("SELECT constraint_text FROM duckdb_constraints() WHERE table_name = 'f'"),
        ["FOREIGN KEY (id) REFERENCES s1.p(id)"]
    );
    db.execute("USE memory.s1").unwrap();
    assert_eq!(values(path), ["s1|memory.s1|[s1]"]);
    assert_eq!(values("SELECT current_schemas(true)"), ["[main, s1, main, main, pg_catalog]"]);
    let error = db.execute("USE system").unwrap_err().to_string();
    assert!(error.contains("cannot be set to internal schema \"system\""), "{error}");
    db.execute("RESET search_path").unwrap();
    assert_eq!(values(path), ["main||[]"]);
    db.execute("USE s2").unwrap();
    db.execute("DROP SCHEMA s2 CASCADE").unwrap();
    assert_eq!(values("SELECT current_schema()"), ["main"]);
}

/// `CREATE TYPE` gives another name to a type, which columns and casts read through, and a type
/// made from another one holds it in place the way the pin says.
#[test]
fn create_type_names_a_type_and_drop_type_follows_the_pin() {
    let db = scripted(&[
        "CREATE TYPE myint AS INTEGER",
        "CREATE TYPE pair AS STRUCT(a INT, b VARCHAR)",
        "CREATE TYPE lst AS myint[]",
        "CREATE SCHEMA s",
        "CREATE TYPE s.st AS VARCHAR",
    ]);
    let values = |sql: &str| -> Vec<String> {
        db.query(sql)
            .unwrap()
            .rows()
            .map(|row| row.iter().map(|v| v.to_string()).collect::<Vec<_>>().join("|"))
            .collect()
    };
    let error = |sql: &str| db.execute(sql).unwrap_err().to_string();
    assert!(
        error("CREATE TYPE myint AS BIGINT").contains("Type with name \"myint\" already exists!")
    );
    assert!(error("CREATE TYPE integer AS VARCHAR").contains("\"integer\" already exists!"));
    db.execute("CREATE TYPE IF NOT EXISTS myint AS BIGINT").unwrap();
    assert!(
        error("CREATE OR REPLACE TYPE myint AS BIGINT").contains("type \"lst\" depends on type")
    );
    db.execute("CREATE TABLE t(a myint, b pair, c lst)").unwrap();
    assert_eq!(
        values("SELECT column_type FROM (DESCRIBE t)"),
        ["INTEGER", "STRUCT(a INTEGER, b VARCHAR)", "INTEGER[]"]
    );
    assert_eq!(values("SELECT typeof(1::myint), typeof(NULL::s.st)"), ["INTEGER|VARCHAR"]);
    assert!(error("SELECT NULL::st").contains("Type with name st does not exist!"));
    assert_eq!(
        values(
            "SELECT schema_name, type_name, logical_type, type_category FROM duckdb_types() \
             WHERE NOT internal"
        ),
        [
            "main|lst|LIST|COMPOSITE",
            "main|myint|INTEGER|NUMERIC",
            "main|pair|STRUCT|COMPOSITE",
            "s|st|VARCHAR|STRING",
        ]
    );
    db.execute("DROP TYPE pair").unwrap();
    assert_eq!(values("SELECT count(*) FROM t"), ["0"]);
    assert!(error("DROP TYPE nope").contains("Type with name nope does not exist!"));
    db.execute("DROP TYPE IF EXISTS nope").unwrap();
    assert!(error("DROP SCHEMA s").contains("type \"st\" depends on schema \"s\"."));
    db.execute("DROP TYPE myint CASCADE").unwrap();
    assert_eq!(values("SELECT type_name FROM duckdb_types() WHERE NOT internal"), ["st"]);
}

/// `CREATE TYPE ... AS ENUM` and the enum functions, per #468.
///
/// Every answer here is the pin's. The ones that matter are the ordering, which is by position in
/// the label list and not by the text, and the comparison with a plain string, which is a string
/// comparison because the pin promotes both sides to VARCHAR.
#[test]
fn enum_types_follow_the_pin() {
    let db = database();
    let text = |s: &str| Value::Varchar(s.to_string());
    db.execute("CREATE TYPE mood AS ENUM ('sad', 'ok', 'happy')").unwrap();
    db.execute("CREATE TABLE moods (m mood)").unwrap();
    db.execute("INSERT INTO moods VALUES ('happy'), ('sad'), (NULL), ('ok')").unwrap();
    assert_eq!(
        rows(&db, "SELECT m FROM moods ORDER BY m"),
        vec![vec![text("sad")], vec![text("ok")], vec![text("happy")], vec![Value::Null]]
    );
    assert_eq!(
        rows(&db, "SELECT min(m), max(m), typeof(min(m)) FROM moods"),
        vec![vec![text("sad"), text("happy"), text("ENUM('sad', 'ok', 'happy')")]]
    );
    assert_eq!(
        rows(&db, "SELECT 'ok'::mood < 'happy'::mood, 'ok'::mood < 'happy'"),
        vec![vec![Value::Boolean(true), Value::Boolean(false)]]
    );
    assert_eq!(
        rows(&db, "SELECT enum_first(NULL::mood), enum_last(NULL::mood), enum_code('ok'::mood)"),
        vec![vec![text("sad"), text("happy"), Value::UTinyInt(1)]]
    );
    assert_eq!(rows(&db, "SELECT upper(m) FROM moods WHERE m = 'ok'"), vec![vec![text("OK")]]);
    let err = db.execute("SELECT 'awesome'::mood").unwrap_err().to_string();
    assert!(err.contains("Could not convert string 'awesome' to UINT8"), "{err}");
    let err = db.execute("CREATE TYPE dup AS ENUM ('a', 'a')").unwrap_err().to_string();
    assert!(err.contains("duplicate value a"), "{err}");
}

/// An enum compared with a number is compared as that number, and a label one enum has and the
/// other lacks names both enums in the error, which are the pin's answers. Per #468.
#[test]
fn an_enum_meets_a_number_and_another_enum_the_way_the_pin_does() {
    let db = database();
    db.execute("CREATE TYPE digits AS ENUM ('1', '2', 'x')").unwrap();
    assert_eq!(
        rows(&db, "SELECT '1'::digits = 1, '2'::digits IN (1, 2)"),
        vec![vec![Value::Boolean(true), Value::Boolean(true)]]
    );
    let err = db.execute("SELECT 'x'::digits = 1").unwrap_err().to_string();
    assert!(err.contains("Could not convert string 'x' to INT32"), "{err}");
    db.execute("CREATE TABLE digit_source (d digits)").unwrap();
    db.execute("INSERT INTO digit_source VALUES ('1'), ('x')").unwrap();
    db.execute("CREATE TABLE digit_sink (d ENUM('1', '2'))").unwrap();
    let err = db.execute("INSERT INTO digit_sink SELECT * FROM digit_source").unwrap_err();
    assert!(
        err.to_string().contains(
            "Type ENUM('1', '2', 'x') with value x can't be cast to the destination type \
             ENUM('1', '2')"
        ),
        "{err}"
    );
    let err = db.execute("SELECT 'a'::ENUM").unwrap_err().to_string();
    assert!(err.contains("ENUM type requires at least one argument"), "{err}");
}
