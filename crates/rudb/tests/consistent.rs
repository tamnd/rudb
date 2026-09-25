//! The consistent extremes rewrite, run with the rule on and off and compared.
//!
//! The rewrite answers a MIN or MAX over an acyclic join of integer equalities by reducing every
//! relation to the rows that take part in some joined row and reading the extremes off those,
//! without running the join. That is only right if the reduction keeps exactly the rows the join
//! would have used, and the places it can go wrong are the ones this file is built around: a null
//! key that the join drops and a set could keep, a null argument that MIN skips, a join that comes
//! out empty and has to answer NULL rather than nothing, duplicate keys that make the join bigger
//! but must not change an extreme, and extremes of strings rather than numbers. Each query runs with
//! `plan.consistent` on and off, and the two answers have to be the same, not similar.
//!
//! The plan side is checked here as well, as much as the SQL can see of it: that the rewrite fires
//! on the shapes it is for, and that the shapes it refuses say why in `EXPLAIN`.

use rudb::Database;
use rudb_common::Value;

/// Three tables in a line, `a` under `b` under `c`, with nulls and duplicates in the keys and in
/// the columns the extremes are read from, plus a fourth that joins nothing so an empty result is
/// easy to reach.
fn database() -> Database {
    let database = Database::new();
    for statement in [
        "CREATE TABLE a (id INTEGER, name VARCHAR, score INTEGER, born BIGINT)",
        "CREATE TABLE b (id INTEGER, a_id INTEGER, c_id BIGINT, note VARCHAR, weight DOUBLE)",
        "CREATE TABLE c (id BIGINT, kind VARCHAR, year INTEGER)",
        "CREATE TABLE lonely (id INTEGER, label VARCHAR)",
        "INSERT INTO a SELECT r::INTEGER, 'name ' || (r % 37)::VARCHAR, \
         CASE WHEN r % 5 = 0 THEN NULL ELSE (r * 13) % 1000 END::INTEGER, (r * 3)::BIGINT \
         FROM range(3000) AS s(r)",
        "INSERT INTO a VALUES (NULL, 'no key', -5, 1), (7, 'again', 99999, 2), (7, 'again', NULL, 3)",
        "INSERT INTO b SELECT r::INTEGER, \
         CASE WHEN r % 11 = 0 THEN NULL ELSE (r * 7) % 4000 END::INTEGER, (r % 50)::BIGINT, \
         CASE WHEN r % 3 = 0 THEN 'odd ' || r::VARCHAR ELSE 'plain' END, (r % 17)::DOUBLE \
         FROM range(5000) AS s(r)",
        "INSERT INTO c SELECT r::BIGINT, CASE WHEN r % 4 = 0 THEN NULL ELSE 'kind ' || (r % 4)::VARCHAR END, \
         (1990 + r % 30)::INTEGER FROM range(40) AS s(r)",
        "INSERT INTO c VALUES (NULL, 'orphan', 1800), (3, 'twin', 2050)",
        "INSERT INTO lonely VALUES (-1, 'nobody'), (NULL, 'nothing')",
    ] {
        database.execute(statement).unwrap_or_else(|error| panic!("{statement} failed: {error}"));
    }
    database
}

/// Every row of a query, as values.
fn rows(database: &Database, sql: &str) -> Vec<Vec<Value>> {
    let result = database.query(sql).unwrap_or_else(|error| panic!("{sql} failed: {error}"));
    (0..result.len())
        .map(|row| (0..result.width()).map(|column| result.value_at(row, column)).collect())
        .collect()
}

/// The plan of a query as `EXPLAIN` prints it, as one string.
fn explain(database: &Database, sql: &str) -> String {
    rows(database, &format!("EXPLAIN {sql}"))
        .into_iter()
        .flatten()
        .map(|value| match value {
            Value::Varchar(text) => text,
            other => format!("{other:?}"),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// The answer of a query with the rule on, after checking it is the answer with the rule off.
fn both(database: &Database, sql: &str) -> Vec<Vec<Value>> {
    let on = rows(database, sql);
    database.execute("SET plan_consistent = false").expect("the rule goes off");
    let off = rows(database, sql);
    database.execute("RESET plan_consistent").expect("and back on");
    assert_eq!(on, off, "the rule changed the answer of {sql}");
    on
}

/// Whether the rewrite fired on a query.
fn fires(database: &Database, sql: &str) -> bool {
    explain(database, sql).contains("Consistent #")
}

/// Queries the rewrite is for. Every one has to fire and every one has to answer as the join does.
const FIRING: &[&str] = &[
    // Two relations, with null keys on both sides and a null argument to skip.
    "SELECT min(a.score), max(b.weight) FROM a, b WHERE a.id = b.a_id",
    // Three in a line, with the extremes read from both ends.
    "SELECT min(a.name), max(c.kind), min(c.year) FROM a JOIN b ON a.id = b.a_id \
     JOIN c ON b.c_id = c.id",
    // The same with filters of every shape attached to one relation each.
    "SELECT min(a.name), max(b.note) FROM a, b, c WHERE a.id = b.a_id AND b.c_id = c.id \
     AND a.name LIKE 'name 1%' AND b.note NOT LIKE '%9' AND c.year BETWEEN 1995 AND 2010 \
     AND (c.kind IS NULL OR c.kind IN ('kind 1', 'kind 3')) AND a.score IS NOT NULL",
    // A star, where the middle table is joined twice and one key is widened by a cast.
    "SELECT max(a.born), min(c.kind) FROM b, a, c WHERE b.a_id = a.id AND c.id = b.c_id \
     AND b.weight > 3",
    // The same table twice, which is duplicates on both sides of every key.
    "SELECT min(x.score), max(y.score) FROM a AS x, a AS y WHERE x.id = y.id",
    // Two trees with nothing between them, a cross product of two joins.
    "SELECT min(a.score), max(c.year) FROM a, b, c, b AS d WHERE a.id = b.a_id AND c.id = d.c_id \
     AND b.id < 40 AND d.id < 40",
    // A key of one width joined to a key of another, which the binder widens with a cast.
    "SELECT min(a.score), max(b.note) FROM a, b WHERE a.born = b.a_id",
    // A join that comes out empty, which answers one row of nulls.
    "SELECT min(a.name), max(lonely.label) FROM a, lonely WHERE a.id = lonely.id",
    // Empty because a filter empties a relation before the join sees it.
    "SELECT min(a.score), max(b.note) FROM a, b WHERE a.id = b.a_id AND b.weight > 100",
    // Every argument null among the surviving rows, which is NULL rather than nothing.
    "SELECT min(a.score), max(a.name) FROM a, b WHERE a.id = b.a_id AND a.id = 5",
    // Behind a projection with names, and under a HAVING, which is a filter above the aggregate.
    "SELECT min(s) AS least, max(t) AS most FROM (SELECT a.score AS s, c.year AS t FROM a \
     JOIN b ON a.id = b.a_id JOIN c ON c.id = b.c_id) AS joined",
    "SELECT max(a.score) FROM a JOIN b ON a.id = b.a_id HAVING max(a.score) > 0",
];

#[test]
fn every_query_it_fires_on_answers_as_the_join_does() {
    let database = database();
    for sql in FIRING {
        assert!(
            fires(&database, sql),
            "the rewrite did not fire on {sql}:\n{}",
            explain(&database, sql)
        );
        both(&database, sql);
    }
}

#[test]
fn the_answers_are_the_ones_the_join_gives() {
    let database = database();
    // The two edge cases worth a literal answer rather than only an agreement: an empty join is one
    // row of nulls, and a key that only matches rows whose argument is null is a null minimum.
    let empty = both(
        &database,
        "SELECT min(a.name), max(lonely.label) FROM a, lonely WHERE a.id = lonely.id",
    );
    assert_eq!(empty, vec![vec![Value::Null, Value::Null]]);
    let skipped = both(&database, "SELECT min(a.score) FROM a, b WHERE a.id = b.a_id AND a.id = 5");
    assert_eq!(skipped, vec![vec![Value::Null]]);
    // Key 7 is in `a` three times, once with a score far above the rest, and it only counts if
    // some row of `b` joins it.
    let joined = both(&database, "SELECT max(a.score) FROM a, b WHERE a.id = b.a_id");
    let direct = rows(
        &database,
        "SELECT max(score) FROM a WHERE id IN (SELECT a_id FROM b WHERE a_id IS NOT NULL)",
    );
    assert_eq!(joined, direct);
}

#[test]
fn the_rule_goes_off_by_either_spelling() {
    let database = database();
    let sql = FIRING[0];
    assert!(fires(&database, sql));
    // The grammar takes a setting name as one identifier, so the dotted spelling is written quoted,
    // the same as it is for the statistics rules.
    database.execute("SET \"plan.consistent\" = false").expect("the dotted spelling");
    assert_eq!(database.setting("plan.consistent").expect("reads back"), "false");
    assert!(!fires(&database, sql), "the rule is off but the rewrite fired");
    database.execute("RESET \"plan.consistent\"").expect("resets");
    assert!(fires(&database, sql));
}

/// Queries the rewrite has to leave alone, each with the words `EXPLAIN` gives as the reason.
const DECLINED: &[(&str, &str)] = &[
    ("SELECT count(*) FROM a, b WHERE a.id = b.a_id", "not a MIN or a MAX"),
    ("SELECT sum(a.score) FROM a, b WHERE a.id = b.a_id", "not a MIN or a MAX"),
    ("SELECT min(a.score), count(*) FROM a, b WHERE a.id = b.a_id", "not a MIN or a MAX"),
    (
        "SELECT string_agg(a.name, ',') FROM a, b WHERE a.id = b.a_id AND a.id < 3",
        "not a MIN or a MAX",
    ),
    (
        "SELECT b.c_id, min(a.score) FROM a, b WHERE a.id = b.a_id GROUP BY b.c_id",
        "groups by a key",
    ),
    ("SELECT min(a.score) FROM a LEFT JOIN b ON a.id = b.a_id", "not an inner join"),
    ("SELECT min(a.score) FROM a, b WHERE a.id < b.a_id AND b.id < 20", "not an equality"),
    ("SELECT min(a.name) FROM a, b WHERE a.name = b.note", "not an equality"),
    (
        "SELECT min(a.score) FROM a, b, c WHERE a.id = b.a_id AND b.c_id = c.id AND c.year = a.score",
        "cycle",
    ),
    ("SELECT min(a.score + 1) FROM a, b WHERE a.id = b.a_id", "other than a column"),
];

#[test]
fn every_shape_it_refuses_says_why_and_answers_as_before() {
    let database = database();
    for (sql, reason) in DECLINED {
        let plan = explain(&database, sql);
        assert!(!plan.contains("Consistent #"), "the rewrite fired on {sql}:\n{plan}");
        assert!(plan.contains("Consistent declined"), "no reason for {sql}:\n{plan}");
        assert!(plan.contains(reason), "{sql} was declined for another reason:\n{plan}");
        both(&database, sql);
    }
}

#[test]
fn an_aggregate_over_one_table_is_left_alone_without_a_word() {
    let database = database();
    let plan = explain(&database, "SELECT min(score), max(name) FROM a WHERE id > 10");
    assert!(!plan.contains("Consistent"), "{plan}");
}
