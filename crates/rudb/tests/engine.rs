//! `SET engine` and the router behind it.
//!
//! With the setting at `compiled`, a query the compiled engine takes runs there and one it refuses
//! runs on the first engine, with the refusal and the statement kept in the log. Either way the
//! rows are the first engine's rows, which is what these tests check.

use rudb::Database;
use rudb_common::Value;

fn database() -> Database {
    let database = Database::new();
    for sql in [
        "CREATE TABLE t (x INTEGER, s VARCHAR)",
        "INSERT INTO t SELECT i % 50 - 10, CASE WHEN i % 7 = 3 THEN NULL ELSE 'w' || (i % 5) END FROM range(3000) r(i)",
    ] {
        database.execute(sql).unwrap_or_else(|error| panic!("{sql} failed: {error}"));
    }
    database
}

fn rows(database: &Database, sql: &str) -> Vec<Vec<Value>> {
    let result = database.query(sql).unwrap_or_else(|error| panic!("{sql} failed: {error}"));
    result.rows().collect()
}

#[test]
fn the_compiled_engine_answers_what_the_first_engine_answers() {
    let database = database();
    let queries = [
        "SELECT count(*), sum(x), min(s), max(s) FROM t",
        "SELECT s, count(*), avg(x) FROM t GROUP BY s ORDER BY s NULLS FIRST",
        "SELECT x + 1, s FROM t WHERE x > 30 ORDER BY 1, 2 LIMIT 7 OFFSET 3",
        "SELECT x, count(DISTINCT s) AS c FROM t GROUP BY x ORDER BY c DESC, x LIMIT 5",
    ];
    for sql in queries {
        database.execute("SET engine = 'first'").expect("the first engine");
        let first = rows(&database, sql);
        database.execute("SET engine = 'compiled'").expect("the compiled engine");
        let compiled = rows(&database, sql);
        assert_eq!(first, compiled, "{sql}");
    }
    assert_eq!(database.refusals(), Vec::<String>::new());
}

#[test]
fn a_refused_query_runs_on_the_first_engine_and_is_logged() {
    let database = database();
    database.execute("SET engine = 'compiled'").expect("the compiled engine");
    let sql = "SELECT count(*) FROM t a JOIN t b ON a.x = b.x WHERE a.x = 3";
    assert_eq!(rows(&database, sql).len(), 1);
    let log = database.refusals();
    assert_eq!(log.len(), 1, "{log:?}");
    assert!(log[0].ends_with(sql), "{log:?}");
}

#[test]
fn the_engine_setting_takes_two_names_and_resets_to_the_first() {
    let database = database();
    assert_eq!(database.setting("engine").expect("a setting"), "first");
    database.execute("SET engine = 'Compiled'").expect("names are not case sensitive");
    assert_eq!(database.setting("engine").expect("a setting"), "compiled");
    let error = database.execute("SET engine = 'fast'").expect_err("no such engine");
    assert!(error.to_string().contains("engine is first or compiled"), "{error}");
    database.execute("RESET engine").expect("reset");
    assert_eq!(database.setting("engine").expect("a setting"), "first");
}

fn explained(database: &Database, sql: &str) -> String {
    match rows(database, sql).as_slice() {
        [row] => match &row[1] {
            Value::Varchar(text) => text.clone(),
            other => panic!("{sql} explained as {other:?}"),
        },
        other => panic!("{sql} explained as {other:?}"),
    }
}

#[test]
fn explain_codegen_prints_the_stages_and_the_module_or_the_refusal() {
    let database = database();
    let text = explained(
        &database,
        "EXPLAIN (CODEGEN) SELECT s, count(*) FROM t GROUP BY s ORDER BY 2 DESC",
    );
    assert!(text.contains("scan "), "{text}");
    assert!(text.contains("aggregate by 1 keys"), "{text}");
    assert!(text.contains("module "), "{text}");
    let text =
        explained(&database, "EXPLAIN (CODEGEN) SELECT count(*) FROM t a JOIN t b ON a.x = b.x");
    assert!(text.starts_with("refused: "), "{text}");
    assert_eq!(
        database.refusals(),
        Vec::<String>::new(),
        "an explain runs nothing, so it logs nothing"
    );
}
