//! A subquery in the condition of a join that is not inner, reading both sides of it. Every
//! expected answer here was taken from the pinned duckdb binary, v2.0.0-dev84237.

use rudb::Database;

fn answered(database: &Database, sql: &str) -> Vec<String> {
    let result = database.query(sql).unwrap_or_else(|error| panic!("{sql} failed: {error}"));
    result
        .rows()
        .map(|row| row.iter().map(ToString::to_string).collect::<Vec<_>>().join("|"))
        .collect()
}

fn tables() -> Database {
    let database = Database::new();
    for sql in [
        "CREATE TABLE pair_l(a INTEGER, x INTEGER)",
        "CREATE TABLE pair_r(b INTEGER, y INTEGER)",
        "CREATE TABLE pair_s(a INTEGER, b INTEGER)",
        "INSERT INTO pair_l VALUES (1, 10), (1, 11), (2, 20), (3, 30)",
        "INSERT INTO pair_r VALUES (10, 100), (20, 200), (30, 300), (40, 400)",
        "INSERT INTO pair_s VALUES (1, 10), (1, 20), (2, 20), (NULL, 30), (3, NULL)",
    ] {
        database.execute(sql).unwrap_or_else(|error| panic!("{sql} failed: {error}"));
    }
    database
}

#[test]
fn each_kind_of_join_keeps_the_rows_it_keeps() {
    let database = tables();
    for (sql, expected) in [
        (
            "SELECT l.a, l.x, r.b FROM pair_l l LEFT JOIN pair_r r ON EXISTS (SELECT 1 FROM \
             pair_s s WHERE s.a = l.a AND s.b = r.b) ORDER BY ALL",
            vec!["1|10|10", "1|10|20", "1|11|10", "1|11|20", "2|20|20", "3|30|NULL"],
        ),
        (
            "SELECT l.a, r.b, r.y FROM pair_l l RIGHT JOIN pair_r r ON r.b IN (SELECT s.b FROM \
             pair_s s WHERE s.a = l.a) ORDER BY ALL",
            vec![
                "1|10|100",
                "1|10|100",
                "1|20|200",
                "1|20|200",
                "2|20|200",
                "NULL|30|300",
                "NULL|40|400",
            ],
        ),
        (
            "SELECT l.a, l.x FROM pair_l l SEMI JOIN pair_r r ON EXISTS (SELECT 1 FROM pair_s s \
             WHERE s.a = l.a AND s.b = r.b) ORDER BY ALL",
            vec!["1|10", "1|11", "2|20"],
        ),
        (
            "SELECT l.a, l.x FROM pair_l l ANTI JOIN pair_r r ON EXISTS (SELECT 1 FROM pair_s s \
             WHERE s.a = l.a AND s.b = r.b) ORDER BY ALL",
            vec!["3|30"],
        ),
        (
            "SELECT l.a, r.b FROM pair_l l FULL OUTER JOIN pair_r r ON EXISTS (SELECT 1 FROM \
             pair_s s WHERE s.a = l.a AND s.b = r.b) ORDER BY ALL",
            vec!["1|10", "1|10", "1|20", "1|20", "2|20", "3|NULL", "NULL|30", "NULL|40"],
        ),
        (
            "SELECT l.a, r.b FROM pair_l l LEFT JOIN pair_r r ON r.y > (SELECT count(*) * 100 \
             FROM pair_s s WHERE s.a = l.a AND s.b <= r.b) ORDER BY ALL",
            vec![
                "1|30", "1|30", "1|40", "1|40", "2|10", "2|20", "2|30", "2|40", "3|10", "3|20",
                "3|30", "3|40",
            ],
        ),
    ] {
        assert_eq!(answered(&database, sql), expected, "{sql}");
    }
}

#[test]
fn a_side_that_is_a_subquery_of_its_own_is_read_through_its_projection() {
    let database = tables();
    let sql = "SELECT l.a, l.x, r.b, r.y FROM (SELECT * FROM pair_l WHERE x >= 10) l FULL OUTER \
               JOIN (SELECT * FROM pair_r WHERE y >= 200) r ON EXISTS (SELECT 1 FROM pair_s s \
               WHERE s.a = l.a AND s.b = r.b) ORDER BY ALL";
    assert_eq!(
        answered(&database, sql),
        [
            "1|10|20|200",
            "1|11|20|200",
            "2|20|20|200",
            "3|30|NULL|NULL",
            "NULL|NULL|30|300",
            "NULL|NULL|40|400",
        ]
    );
}

#[test]
fn the_join_can_itself_be_inside_a_subquery_that_reads_an_outer_row() {
    let database = tables();
    for (sql, expected) in [
        (
            "SELECT o.a, (SELECT count(*) FROM pair_l l LEFT JOIN pair_r r ON EXISTS (SELECT 1 \
             FROM pair_s s WHERE s.a = l.a AND s.b = r.b AND s.a = o.a)) FROM pair_l o ORDER BY \
             ALL",
            vec!["1|6", "1|6", "2|4", "3|4"],
        ),
        (
            "SELECT o.a FROM pair_l o WHERE o.a IN (SELECT l.a FROM pair_l l SEMI JOIN pair_r r \
             ON EXISTS (SELECT 1 FROM pair_s s WHERE s.a = l.a AND s.b = r.b AND r.b > o.x)) \
             ORDER BY ALL",
            vec!["1", "1"],
        ),
    ] {
        assert_eq!(answered(&database, sql), expected, "{sql}");
    }
}

#[test]
fn a_side_with_nextval_in_it_runs_once() {
    let database = Database::new();
    for sql in [
        "CREATE SEQUENCE q",
        "CREATE TABLE l(a INTEGER)",
        "CREATE TABLE r(b INTEGER)",
        "INSERT INTO l VALUES (1), (2)",
        "INSERT INTO r VALUES (1), (2), (3)",
    ] {
        database.execute(sql).unwrap_or_else(|error| panic!("{sql} failed: {error}"));
    }
    assert_eq!(
        answered(
            &database,
            "SELECT count(*), currval('q') FROM (SELECT a, nextval('q') AS v FROM l) l SEMI JOIN \
             r ON EXISTS (SELECT 1 WHERE l.v = r.b)"
        ),
        ["2|2"]
    );
    database.execute("DROP SEQUENCE q").expect("the drop");
    database.execute("CREATE SEQUENCE q").expect("the sequence again");
    assert_eq!(
        answered(
            &database,
            "SELECT l.a, l.v, r.b FROM (SELECT a, nextval('q') AS v FROM l) l FULL JOIN r ON \
             EXISTS (SELECT 1 WHERE l.v = r.b) ORDER BY ALL"
        ),
        ["1|1|1", "2|2|2", "NULL|NULL|3"]
    );
}
