//! `WITH RECURSIVE ... USING KEY`, and `recurring.` reads, from the answers.
//!
//! Every answer and every error asserted here was read off the pinned duckdb on server2 first,
//! which is v2.0.0-dev84237 at cc7e7bac7f. With a key the rows a recursion produces are a table
//! keyed on the key columns, where a later row for a key replaces the earlier one, and a
//! `recurring.` read sees that table as it stood when the round began. An aggregate in the key
//! keeps a running answer per key in a column of the table instead.

use rudb::Database;

/// Every row a query answers with, each as its cells joined with a bar, in the order they came.
fn rows(database: &Database, sql: &str) -> Vec<String> {
    let result = database.query(sql).unwrap_or_else(|error| panic!("{sql} failed: {error}"));
    (0..result.len())
        .map(|row| {
            (0..result.width())
                .map(|column| result.text_at(row, column))
                .collect::<Vec<_>>()
                .join("|")
        })
        .collect()
}

/// The error a query gives, up to the end of its first line.
fn refused(database: &Database, sql: &str) -> String {
    let error = database.query(sql).expect_err(&format!("{sql} should be refused")).to_string();
    error.lines().next().unwrap_or_default().to_string()
}

#[test]
fn a_later_row_for_a_key_replaces_the_earlier_one() {
    let database = Database::new();
    for word in ["UNION", "UNION ALL"] {
        assert_eq!(
            rows(
                &database,
                &format!(
                    "WITH RECURSIVE tbl(a, b) USING KEY (a) AS (SELECT a, b FROM (VALUES (1, 3), \
                     (2, 4)) t(a, b) {word} SELECT a + 1, b FROM tbl WHERE a < 3) \
                     SELECT * FROM tbl ORDER BY a"
                )
            ),
            ["1|3", "2|3", "3|3"],
            "{word}"
        );
    }
    // The last row wins, in the anchor and in a round alike.
    for (anchor, expected) in [("(1, 1), (1, 2)", "1|2"), ("(1, 2), (1, 1)", "1|1")] {
        assert_eq!(
            rows(
                &database,
                &format!(
                    "WITH RECURSIVE t(k, v) USING KEY (k) AS (VALUES {anchor} UNION ALL \
                     SELECT k, v FROM t WHERE false) SELECT * FROM t"
                )
            ),
            [expected]
        );
    }
    for (offered, expected) in [("(5), (6)", "1|6"), ("(6), (5)", "1|5")] {
        assert_eq!(
            rows(
                &database,
                &format!(
                    "WITH RECURSIVE t(k, v) USING KEY (k) AS (SELECT 1, 0 UNION ALL SELECT k, x \
                     FROM t, (VALUES {offered}) v(x) WHERE t.v = 0) SELECT * FROM t"
                )
            ),
            [expected]
        );
    }
    assert_eq!(
        rows(
            &database,
            "WITH RECURSIVE t(k, v) USING KEY (k) AS (SELECT 1, 0 UNION ALL SELECT k, v + 1 FROM t \
             WHERE v < 3) SELECT * FROM t"
        ),
        ["1|3"]
    );
    // The key can be any column, and the rows come out in the order their keys first came.
    assert_eq!(
        rows(
            &database,
            "WITH RECURSIVE t(k, v) USING KEY (v) AS (SELECT 1, 0 UNION ALL SELECT k + 1, v FROM t \
             WHERE k < 3) SELECT * FROM t"
        ),
        ["3|0"]
    );
    assert_eq!(
        rows(
            &database,
            "WITH RECURSIVE t(k, v) USING KEY (k) AS (SELECT 3, 0 UNION ALL SELECT k - 1, v FROM t \
             WHERE k > 1) SELECT * FROM t"
        ),
        ["3|0", "2|0", "1|0"]
    );
    assert_eq!(
        rows(
            &database,
            "WITH RECURSIVE t(k, v) USING KEY (k) AS (SELECT NULL::INT, 0 UNION ALL \
             SELECT NULL::INT, v + 1 FROM t WHERE v < 2) SELECT * FROM t"
        ),
        ["NULL|2"]
    );
    assert_eq!(
        rows(
            &database,
            "WITH RECURSIVE t(k, v) USING KEY (k) AS (SELECT 1, 0 UNION ALL SELECT k, v + 1.5 \
             FROM t WHERE v < 3) SELECT * FROM t"
        ),
        ["1|4"]
    );
    assert_eq!(
        rows(
            &database,
            "WITH RECURSIVE t(k, v) USING KEY (k) AS (SELECT 1, 0 UNION ALL SELECT k + 1, v FROM t \
             WHERE k < 3), u AS (SELECT count(*) AS c FROM t) SELECT * FROM u"
        ),
        ["3"]
    );
}

#[test]
fn with_all_a_round_passes_on_every_row_and_without_it_one_changed_row_per_key() {
    let database = Database::new();
    assert_eq!(
        rows(
            &database,
            "WITH RECURSIVE t(k, v) USING KEY (k) AS (VALUES (1, 1), (1, 2) UNION ALL \
             SELECT k + 1, count(*) FROM t WHERE k < 2 GROUP BY k) SELECT * FROM t ORDER BY k"
        ),
        ["1|2", "2|2"]
    );
    assert_eq!(
        rows(
            &database,
            "WITH RECURSIVE t(k, v) USING KEY (k) AS (VALUES (1, 1), (1, 2) UNION \
             SELECT k + 1, count(*) FROM t WHERE k < 2 GROUP BY k) SELECT * FROM t ORDER BY k"
        ),
        ["1|2", "2|1"]
    );
    // A key given back the value it had is no work, so these stop.
    assert_eq!(
        rows(
            &database,
            "WITH RECURSIVE t(k, v) USING KEY (k) AS (VALUES (1, 0) UNION SELECT k, v FROM t) \
             TABLE t"
        ),
        ["1|0"]
    );
    assert_eq!(
        rows(
            &database,
            "WITH RECURSIVE t(k, v) USING KEY (k) AS (VALUES (1, NULL::INTEGER) UNION \
             SELECT k, v FROM t) TABLE t"
        ),
        ["1|NULL"]
    );
    assert_eq!(
        rows(
            &database,
            "WITH RECURSIVE t(k, v) USING KEY (k) AS (VALUES (1, 'NaN'::DOUBLE), \
             (2, -0e0::DOUBLE) UNION SELECT k, CASE WHEN k = 1 THEN 'NaN'::DOUBLE ELSE 0e0::DOUBLE \
             END FROM t) SELECT count(*), count_if(isnan(v)), count_if(signbit(v)) FROM t"
        ),
        ["2|1|0"]
    );
    assert_eq!(
        rows(
            &database,
            "WITH RECURSIVE t(k, a, b) USING KEY (k) AS (VALUES (1, 0, 0), (2, 0, 0) UNION \
             SELECT k, CASE WHEN k = 1 THEN 1 ELSE a END, CASE WHEN k = 2 THEN b + 1 ELSE b END \
             FROM t WHERE (k = 1 AND a = 0) OR (k = 2 AND b < 2)) SELECT * FROM t ORDER BY k"
        ),
        ["1|1|0", "2|0|2"]
    );
    assert_eq!(
        rows(
            &database,
            "WITH RECURSIVE t(k, v) USING KEY (k) AS (VALUES (1, 10) UNION SELECT k + 1, v + 10 \
             FROM t WHERE k < 4) SELECT * FROM t ORDER BY k"
        ),
        ["1|10", "2|20", "3|30", "4|40"]
    );
}

#[test]
fn a_recurring_read_sees_the_table_as_the_round_began() {
    let database = Database::new();
    for word in ["UNION", "UNION ALL"] {
        assert_eq!(
            rows(
                &database,
                &format!(
                    "WITH RECURSIVE t(k, v) USING KEY (k) AS (SELECT 1, 0 {word} SELECT 1, 1 \
                     FROM recurring.t WHERE v < 1) SELECT * FROM t"
                )
            ),
            ["1|1"],
            "{word}"
        );
    }
    assert_eq!(
        rows(
            &database,
            "WITH RECURSIVE t(k, v) USING KEY (k) AS (SELECT 1, 0 UNION ALL SELECT n.k, r.v + 1 \
             FROM t n JOIN recurring.t r USING (k) WHERE n.v < 3) SELECT k, v FROM t"
        ),
        ["1|3"]
    );
    assert_eq!(
        rows(
            &database,
            "WITH RECURSIVE t(k, v) USING KEY (k) AS (SELECT i, 0 FROM range(8192) r(i) UNION ALL \
             SELECT n.k, r.v + 1 FROM t n JOIN recurring.t r USING (k) WHERE n.v < 3) \
             SELECT count(*), min(v), max(v), sum(v) FROM t"
        ),
        ["8192|3|3|24576"]
    );
    assert_eq!(
        rows(
            &database,
            "WITH RECURSIVE t(k, v) USING KEY (k) AS (SELECT 1, 0 UNION SELECT r.k + 1, r.v \
             FROM t, recurring.t AS r WHERE t.k < 2) SELECT * FROM t ORDER BY k"
        ),
        ["1|0", "2|0"]
    );
    assert_eq!(
        rows(
            &database,
            "WITH RECURSIVE t(k, v) USING KEY (k) AS (SELECT 1, 0 UNION SELECT k + 1, v \
             FROM RECURRING.T WHERE k < 3) SELECT * FROM t ORDER BY k"
        ),
        ["1|0", "2|0", "3|0"]
    );
    // Without a key it reads every row produced so far.
    assert_eq!(
        rows(
            &database,
            "WITH RECURSIVE t(k, v) AS (SELECT 1, 0 UNION SELECT k + 1, v FROM recurring.t \
             WHERE k < 3) SELECT * FROM t ORDER BY k"
        ),
        ["1|0", "2|0", "3|0"]
    );
    assert_eq!(
        rows(
            &database,
            "WITH RECURSIVE t(k, v) AS (SELECT 1, 0 UNION ALL SELECT t.k + 1, r.v FROM t, \
             recurring.t r WHERE t.k < 3) SELECT count(*) FROM t"
        ),
        ["4"]
    );
}

#[test]
fn a_union_under_a_key_takes_one_row_per_key_even_when_it_does_not_read_itself() {
    let database = Database::new();
    // The pin plans this as the last row per key over both sides, in whatever order the two sides
    // reach the grouping, so either side can win and only the shape of the answer is fixed.
    for word in ["UNION", "UNION ALL"] {
        assert_eq!(
            rows(
                &database,
                &format!(
                    "WITH RECURSIVE t(k, v) USING KEY (k) AS (VALUES (1, 1), (1, 2) {word} \
                     VALUES (1, 3)) SELECT count(*), min(v) = max(v), min(v) IN (2, 3) FROM t"
                )
            ),
            ["1|true|true"],
            "{word}"
        );
    }
    assert_eq!(
        rows(
            &database,
            "WITH RECURSIVE t(k, v) USING KEY (k) AS (VALUES (1, 1) UNION VALUES (2, 3), (2, 4)) \
             SELECT * FROM t ORDER BY k"
        ),
        ["1|1", "2|4"]
    );
    assert_eq!(
        rows(
            &database,
            "WITH RECURSIVE t(k) USING KEY (k) AS (SELECT 2 UNION SELECT 1 UNION SELECT 3) \
             SELECT * FROM t ORDER BY k"
        ),
        ["1", "2", "3"]
    );
    // Without a key, or without a union, it is the plain definition it looks like.
    assert_eq!(
        rows(&database, "WITH RECURSIVE t(k) AS (SELECT 2 UNION ALL SELECT 2) SELECT * FROM t"),
        ["2", "2"]
    );
    assert_eq!(
        rows(
            &database,
            "WITH RECURSIVE t(k, v) USING KEY (k) AS (VALUES (1, 1), (1, 2) EXCEPT \
             VALUES (1, 3)) SELECT * FROM t ORDER BY v"
        ),
        ["1|1", "1|2"]
    );
    assert!(
        refused(
            &database,
            "WITH RECURSIVE t(k, v) USING KEY (zz) AS (VALUES (1, 1) UNION VALUES (1, 3)) \
             SELECT * FROM t"
        )
        .starts_with("Binder Error: Referenced column \"zz\" not found in FROM clause!")
    );
}

#[test]
fn a_view_keeps_the_key_and_the_recurring_read() {
    let database = Database::new();
    database
        .execute(
            "CREATE VIEW vv AS WITH RECURSIVE t(k, v) USING KEY (k) AS (SELECT 1, 0 UNION \
             SELECT k, v + 1 FROM recurring.t WHERE v < 2) SELECT * FROM t",
        )
        .expect("the view");
    assert_eq!(
        rows(&database, "SELECT sql FROM duckdb_views() WHERE view_name = 'vv'"),
        ["CREATE VIEW vv AS WITH RECURSIVE t (k, v) USING KEY (k)  AS ((SELECT 1, 0) UNION \
             (SELECT k, (v + 1) FROM recurring.t WHERE (v < 2)))SELECT * FROM t;"]
    );
    assert_eq!(rows(&database, "SELECT * FROM vv"), ["1|2"]);
    database
        .execute(
            "CREATE VIEW v1 AS WITH RECURSIVE t(k, v) USING KEY (k) AS (VALUES (1, 1) UNION \
             VALUES (1, 3)) SELECT * FROM t",
        )
        .expect("the view");
    assert_eq!(
        rows(&database, "SELECT sql FROM duckdb_views() WHERE view_name = 'v1'"),
        ["CREATE VIEW v1 AS WITH RECURSIVE t (k, v) USING KEY (k)  AS ((SELECT * FROM \
             (VALUES (1, 1)) AS valueslist) UNION (SELECT * FROM (VALUES (1, 3)) AS \
             valueslist))SELECT * FROM t;"]
    );
}

#[test]
fn what_the_pin_refuses_is_refused_in_its_words() {
    let database = Database::new();
    assert!(
        refused(
            &database,
            "WITH RECURSIVE t(k, v) USING KEY (zz) AS (SELECT 1, 0 UNION ALL SELECT k + 1, v \
             FROM t WHERE k < 3) SELECT * FROM t"
        )
        .starts_with("Binder Error: Referenced column \"zz\" not found in FROM clause!")
    );
    assert_eq!(
        refused(
            &database,
            "WITH RECURSIVE t(k, v) USING KEY (k + 1) AS (SELECT 1, 0 UNION ALL SELECT k + 1, v \
             FROM t WHERE k < 3) SELECT * FROM t"
        ),
        "Binder Error: '(k + 1)' can't be used in the USING KEY clause. It has to be either a \
         column name as a key or a direct call to an aggregate function."
    );
    assert_eq!(
        refused(
            &database,
            "WITH RECURSIVE t(k, v) USING KEY (k) AS (SELECT * FROM recurring.t UNION \
             SELECT k + 1, v FROM t WHERE k < 3) SELECT * FROM t ORDER BY k"
        ),
        "Catalog Error: Table with name \"recurring.t\" does not exist because schema \
         \"recurring\" does not exist."
    );
    // A key on a definition that does not read itself is never looked at.
    assert_eq!(
        rows(&database, "WITH RECURSIVE t(k) USING KEY (zz) AS (SELECT 1) SELECT * FROM t"),
        ["1"]
    );
    assert_eq!(
        rows(&database, "WITH t(k, v) USING KEY (k) AS (VALUES (1, 1), (1, 2)) SELECT * FROM t"),
        ["1|1", "1|2"]
    );
}

#[test]
fn an_aggregate_keeps_a_running_answer_per_key() {
    let database = Database::new();
    for word in ["UNION", "UNION ALL"] {
        assert_eq!(
            rows(
                &database,
                &format!(
                    "WITH RECURSIVE t(a, b) USING KEY (a, max(b)) AS (SELECT 1, 5 {word} \
                     SELECT a, b - 1 FROM t WHERE b > 0) SELECT * FROM t"
                )
            ),
            ["1|5"],
            "{word}"
        );
        assert_eq!(
            rows(
                &database,
                &format!(
                    "WITH RECURSIVE t(k, v, n) USING KEY (k, sum(v)) AS (SELECT 1, 1, 0 {word} \
                     SELECT k, 1, n + 1 FROM t WHERE n < 4) SELECT * FROM t"
                )
            ),
            ["1|5|4"],
            "{word}"
        );
    }
    for (key, expected) in [
        ("k, avg(v)", "1|1.0"),
        ("k, list(v)", "1|[0, 1, 2, 3]"),
        ("sum(v), k", "1|6"),
        ("k, sum(t.v)", "1|6"),
        ("k, sum(V)", "1|6"),
        ("k, sum(v) AS V", "1|6"),
        ("k, max(v)", "1|3"),
    ] {
        let anchor = if key.contains("avg") { "SELECT 1, 2" } else { "SELECT 1, 0" };
        let step = if key.contains("avg") {
            "v - 1 FROM t WHERE v > 0"
        } else {
            "v + 1 FROM t WHERE v < 3"
        };
        assert_eq!(
            rows(
                &database,
                &format!(
                    "WITH RECURSIVE t(k, v) USING KEY ({key}) AS ({anchor} UNION ALL \
                     SELECT k, {step}) SELECT * FROM t"
                )
            ),
            [expected],
            "{key}"
        );
    }
    // The other columns take the last row's values, and an aggregate can land in any column.
    assert_eq!(
        rows(
            &database,
            "WITH RECURSIVE t(k, v, w) USING KEY (k, arg_min(w, v)) AS (SELECT 1, 0, 5 \
             UNION ALL SELECT k, v + 1, w - 1 FROM t WHERE v < 3) SELECT * FROM t"
        ),
        ["1|3|5"]
    );
    assert_eq!(
        rows(
            &database,
            "WITH RECURSIVE t(k, v, c) USING KEY (k, sum(v) AS v, count(v) AS c) AS \
             (SELECT 1, 0, 0 UNION ALL SELECT k, v + 1, 0 FROM t WHERE v < 3) SELECT * FROM t"
        ),
        ["1|6|4"]
    );
    // A key named after an aggregate already landed in its column is a key like any other.
    assert_eq!(
        rows(
            &database,
            "WITH RECURSIVE t(k, v) USING KEY (k, max(v), v) AS (SELECT 1, 0 UNION ALL \
             SELECT k, v + 1 FROM t WHERE v < 3) SELECT * FROM t"
        ),
        ["1|0", "1|1", "1|2", "1|3"]
    );
    assert_eq!(
        rows(
            &database,
            "WITH RECURSIVE t(k, v) USING KEY (k, min(v)) AS (SELECT k, 10 FROM range(3) r(k) \
             UNION SELECT k, v - k - 1 FROM t WHERE v > 5) SELECT * FROM t ORDER BY k"
        ),
        ["0|5", "1|4", "2|4"]
    );
    for (aggregate, expected) in [("count", "1|0"), ("sum", "1|NULL")] {
        assert_eq!(
            rows(
                &database,
                &format!(
                    "WITH RECURSIVE t(k, v) USING KEY (k, {aggregate}(v)) AS (SELECT 1, \
                     NULL::INT UNION ALL SELECT k, 1 FROM t WHERE false) SELECT * FROM t"
                )
            ),
            [expected]
        );
    }
}

#[test]
fn a_round_reads_the_rows_made_with_all_and_the_finished_rows_without_it() {
    let database = Database::new();
    // The table holds the answer's type, which a round reads only without `ALL`.
    assert_eq!(
        rows(
            &database,
            "WITH RECURSIVE t(k, v) USING KEY (k, sum(v)) AS (SELECT 1, 0 UNION ALL SELECT k, \
             v + 1 FROM t WHERE v < 3) SELECT typeof(v), v FROM t"
        ),
        ["HUGEINT|6"]
    );
    assert_eq!(
        rows(
            &database,
            "WITH RECURSIVE t(k, v) USING KEY (k, sum(v)) AS (SELECT 1, 0 UNION ALL SELECT k, \
             v + 1 FROM t WHERE v < 3 AND typeof(v) = 'INTEGER') SELECT v FROM t"
        ),
        ["6"]
    );
    assert_eq!(
        rows(
            &database,
            "WITH RECURSIVE t(k, v) USING KEY (k, sum(v)) AS (SELECT 1, 0 UNION SELECT k, v + 1 \
             FROM t WHERE v < 3 AND typeof(v) = 'HUGEINT') SELECT v FROM t"
        ),
        ["3"]
    );
    assert_eq!(
        rows(
            &database,
            "WITH RECURSIVE t(k, v) USING KEY (k, max(v)) AS (SELECT 1, 0 UNION SELECT r.k, \
             r.v + 1 FROM recurring.t r WHERE r.v < 3) SELECT * FROM t"
        ),
        ["1|3"]
    );
    // Without a read of itself the right side runs once.
    assert_eq!(
        rows(
            &database,
            "WITH RECURSIVE t(k, v) USING KEY (k, sum(v)) AS (SELECT 1, 1 UNION ALL SELECT 1, 2) \
             SELECT typeof(v), v FROM t"
        ),
        ["HUGEINT|3"]
    );
    assert_eq!(
        rows(
            &database,
            "WITH RECURSIVE t(k, v) USING KEY (k, sum(v)) AS (SELECT 1, 0 UNION ALL SELECT k, \
             v + 1 FROM t WHERE v < 3), u AS (SELECT * FROM t) SELECT typeof(v), v FROM u"
        ),
        ["HUGEINT|6"]
    );
}

#[test]
fn an_aggregate_the_pin_refuses_is_refused_in_its_words() {
    let database = Database::new();
    for (key, expected) in [
        (
            "k, max(v + 1)",
            "In USING KEY, an aggregate must either have a column reference or an alias.",
        ),
        (
            "k, count(*)",
            "In USING KEY, an aggregate must either have a column reference or an alias.",
        ),
        (
            "k, arg_min(1, v)",
            "In USING KEY, an aggregate must either have a column reference or an alias.",
        ),
        ("k, sum(v) AS w", "Could not find column with name '\"w\"' to bind aggregate to."),
        (
            "k, sum(k)",
            "Column '\"k\"' cannot be used as both key and aggregate in USING KEY clause. Try \
             using an alias for the aggregation.",
        ),
        (
            "k, sum(v) AS k",
            "Column '\"k\"' cannot be used as both key and aggregate in USING KEY clause. Try \
             using an alias for the aggregation.",
        ),
        ("sum(v)", "USING KEY clause requires at least one key column."),
        (
            "k, sum(v), max(v)",
            "Column '\"v\"' referenced multiple times in USING KEY clause. Try using an alias for \
             one of the aggregates.",
        ),
        (
            "k, sum(v) FILTER (WHERE v > 1)",
            "FILTER clause is not yet supported for aggregates in USING KEY",
        ),
        (
            "k, sum(v) FILTER (WHERE zz)",
            "FILTER clause is not yet supported for aggregates in USING KEY",
        ),
        ("k, sum(DISTINCT v)", "DISTINCT is not yet supported for aggregates in USING KEY"),
        (
            "k, list(v ORDER BY v DESC)",
            "ORDER BY clause is not yet supported for aggregates in USING KEY",
        ),
        ("k, sum(sum(v))", "Aggregate functions are not supported here"),
        (
            "k, abs(v)",
            "'abs(v)' can't be used in the USING KEY clause. It has to be either a column name as \
             a key or a direct call to an aggregate function.",
        ),
        (
            "k AS v, max(v)",
            "In USING KEY, only direct calls to an aggregate function can have an alias.",
        ),
        (
            "k, avg(*)",
            "In USING KEY, an aggregate must either have a column reference or an alias.",
        ),
        ("k, avg(*) AS v", "No matching aggregate function"),
        ("k, avg(* :: INT) AS v", "STAR expression is not supported here"),
        (
            "k, memory.main.max(v)",
            "'memory.main.max(v)' can't be used in the USING KEY clause. It has to be either a \
             column name as a key or a direct call to an aggregate function.",
        ),
        (
            "k, zzagg(v)",
            "'zzagg(v)' can't be used in the USING KEY clause. It has to be either a column name \
             as a key or a direct call to an aggregate function.",
        ),
    ] {
        assert_eq!(
            refused(
                &database,
                &format!(
                    "WITH RECURSIVE t(k, v) USING KEY ({key}) AS (SELECT 1, 0 UNION ALL \
                     SELECT k, v + 1 FROM t WHERE v < 3) SELECT * FROM t"
                )
            ),
            format!("Binder Error: {expected}"),
            "{key}"
        );
    }
    // A schema in front of the name is not looked at, and neither is the catalog when it is
    // `system`.
    for key in ["k, system.main.max(v)", "k, zz.max(v)", "k, system.zz.max(v)"] {
        assert_eq!(
            rows(
                &database,
                &format!(
                    "WITH RECURSIVE r(k, v) USING KEY ({key}) AS (VALUES (1, 10) UNION ALL \
                     SELECT k + 1, v + 1 FROM r WHERE k < 3) SELECT * FROM r ORDER BY k"
                )
            ),
            ["1|10", "2|11", "3|12"],
            "{key}"
        );
    }
    assert_eq!(
        rows(
            &database,
            "WITH RECURSIVE t(a, b) USING KEY (a, count(*) AS b) AS (SELECT 1, 1 UNION ALL \
             SELECT a, b + 1 FROM t WHERE b < 5) TABLE t"
        ),
        ["1|5"]
    );
    for key in ["k, max(zz) AS v", "k, max(v + zz)", "k, sum(zz) AS v, max(v)"] {
        assert!(
            refused(
                &database,
                &format!(
                    "WITH RECURSIVE t(k, v) USING KEY ({key}) AS (SELECT 1, 0 UNION ALL \
                     SELECT k, v + 1 FROM t WHERE v < 3) SELECT * FROM t"
                )
            )
            .starts_with("Binder Error: Referenced column \"zz\" not found in FROM clause!"),
            "{key}"
        );
    }
}

#[test]
fn a_key_keeps_the_value_it_was_first_stored_with() {
    let database = Database::new();
    for (word, key) in [("UNION ALL", "k"), ("UNION", "k"), ("UNION ALL", "k, max(v)")] {
        assert_eq!(
            rows(
                &database,
                &format!(
                    "WITH RECURSIVE t(k, v) USING KEY ({key}) AS (VALUES ('-0'::DOUBLE, 0) \
                     {word} SELECT '0'::DOUBLE, v + 1 FROM t WHERE v < 2) \
                     SELECT signbit(k), v FROM t"
                )
            ),
            ["true|2"],
            "{word} {key}"
        );
    }
    assert_eq!(
        rows(
            &database,
            "WITH RECURSIVE t(k, v) USING KEY (k) AS (VALUES ('-0'::DOUBLE, 0), ('0'::DOUBLE, 5) \
             UNION ALL SELECT k, v FROM t WHERE false) SELECT signbit(k), v FROM t"
        ),
        ["true|5"]
    );
}

#[test]
fn a_definition_nothing_reads_is_never_bound() {
    let database = Database::new();
    for sql in [
        "WITH RECURSIVE t AS (SELECT zz UNION ALL SELECT * FROM t) SELECT 1",
        "WITH RECURSIVE t(k, v) USING KEY (k, sum(zz)) AS (SELECT 1, 0 UNION ALL SELECT k, v \
         FROM t) SELECT 1",
    ] {
        assert_eq!(rows(&database, sql), ["1"], "{sql}");
    }
}
