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
        "SELECT replace(s, 'w', 'a longer string '), abs(x) % 7, concat(s, '!') FROM t ORDER BY 1, 2, 3 LIMIT 20",
        "SELECT x, x - 1, x * 2, count(*) AS c FROM t GROUP BY x, x - 1, x * 2 ORDER BY c DESC, x LIMIT 5",
        "SELECT count(*), sum(x), min(s) FROM t WHERE x = 3 AND s <> 'w1'",
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
fn rows_with_no_order_come_back_in_file_order_from_many_workers_on_the_compiled_engine() {
    let database = Database::new();
    for sql in [
        "SET threads = 8",
        "CREATE TABLE big (i BIGINT, s VARCHAR)",
        "INSERT INTO big SELECT i, 'row ' || i FROM range(400000) r(i)",
    ] {
        database.execute(sql).unwrap_or_else(|error| panic!("{sql} failed: {error}"));
    }
    for sql in
        ["SELECT i, s FROM big WHERE i % 1000 = 7", "SELECT i * 2, s FROM big WHERE s LIKE '%99%'"]
    {
        database.execute("SET engine = 'first'").expect("the first engine");
        let first = rows(&database, sql);
        database.execute("SET engine = 'compiled'").expect("the compiled engine");
        for _ in 0..3 {
            assert_eq!(first, rows(&database, sql), "{sql}");
        }
    }
    assert_eq!(database.refusals(), Vec::<String>::new());
}

#[test]
fn a_limit_with_no_order_over_groups_reads_whole_groups_on_the_compiled_engine() {
    let database = database();
    let whole = rows(&database, "SELECT x, s, count(*), sum(x), min(s) FROM t GROUP BY x, s");
    database.execute("SET engine = 'compiled'").expect("the compiled engine");
    for sql in [
        "SELECT x, s, count(*), sum(x), min(s) FROM t GROUP BY x, s LIMIT 7",
        "SELECT x, s, count(*), sum(x), min(s) FROM t GROUP BY x, s LIMIT 4 OFFSET 3",
    ] {
        let some = rows(&database, sql);
        assert_eq!(some.len(), if sql.contains("OFFSET") { 4 } else { 7 }, "{sql}");
        for row in &some {
            assert!(whole.contains(row), "{sql} made {row:?}");
        }
    }
    assert_eq!(database.refusals(), Vec::<String>::new());
}

#[test]
fn a_top_n_by_a_count_picks_its_groups_by_the_count_on_the_compiled_engine() {
    let database = Database::new();
    for sql in [
        "CREATE TABLE u (k INTEGER, v VARCHAR)",
        // Key k has k rows, so no two groups tie on the count.
        "INSERT INTO u SELECT a.i, 'v' || (b.j % 3) FROM range(100) a(i), range(100) b(j) WHERE b.j < a.i",
    ] {
        database.execute(sql).unwrap_or_else(|error| panic!("{sql} failed: {error}"));
    }
    for sql in [
        "SELECT k, count(*) AS c FROM u GROUP BY k ORDER BY c DESC LIMIT 3",
        "SELECT k, count(v) AS c FROM u GROUP BY k ORDER BY c LIMIT 4 OFFSET 2",
        "SELECT v, k, count(*) AS c FROM u GROUP BY v, k ORDER BY c DESC, v, k LIMIT 5",
    ] {
        database.execute("SET engine = 'first'").expect("the first engine");
        let first = rows(&database, sql);
        database.execute("SET engine = 'compiled'").expect("the compiled engine");
        assert_eq!(rows(&database, sql), first, "{sql}");
    }
    assert_eq!(database.refusals(), Vec::<String>::new());
}

#[test]
fn a_top_n_over_a_scan_tells_the_scan_its_cutoff_on_the_compiled_engine() {
    let database = Database::new();
    for sql in [
        "CREATE TABLE t (k BIGINT, s VARCHAR)",
        // Enough rows for many parts, with the keys spread so that most parts hold none of the top.
        "INSERT INTO t SELECT (i * 7919) % 300000, CASE WHEN i % 5 = 0 THEN '' ELSE 's' || (i % 97) END FROM range(300000) r(i)",
    ] {
        database.execute(sql).unwrap_or_else(|error| panic!("{sql} failed: {error}"));
    }
    for sql in [
        "SELECT s FROM t WHERE s <> '' ORDER BY k LIMIT 10",
        "SELECT s, k FROM t WHERE s <> '' ORDER BY k DESC LIMIT 5 OFFSET 3",
        "SELECT k, s FROM t ORDER BY k, s LIMIT 7",
    ] {
        database.execute("SET engine = 'first'").expect("the first engine");
        let first = rows(&database, sql);
        database.execute("SET engine = 'compiled'").expect("the compiled engine");
        assert_eq!(rows(&database, sql), first, "{sql}");
    }
    assert_eq!(database.refusals(), Vec::<String>::new());
}

#[test]
fn a_whole_table_aggregate_the_statistics_answer_matches_on_the_compiled_engine() {
    let database = Database::new();
    for sql in [
        "CREATE TABLE w (a SMALLINT, b BIGINT, d DATE)",
        "INSERT INTO w SELECT i % 4, i * 3 - 1000, DATE '2020-01-01' + (i % 400)::INTEGER FROM range(50000) r(i)",
        "INSERT INTO w VALUES (NULL, NULL, NULL)",
    ] {
        database.execute(sql).unwrap_or_else(|error| panic!("{sql} failed: {error}"));
    }
    for sql in [
        "SELECT count(*) FROM w",
        "SELECT count(*) FROM w WHERE a <> 0",
        "SELECT count(*) FROM w WHERE a = 2",
        "SELECT count(b), sum(b), min(b), max(b) FROM w",
        "SELECT min(d), max(d) FROM w",
        "SELECT count(DISTINCT a) FROM w",
        "SELECT sum(a), avg(b) FROM w WHERE b > 0",
    ] {
        database.execute("SET engine = 'first'").expect("the first engine");
        let first = rows(&database, sql);
        database.execute("SET engine = 'compiled'").expect("the compiled engine");
        assert_eq!(rows(&database, sql), first, "{sql}");
    }
}

#[test]
fn a_refused_query_runs_on_the_first_engine_and_is_logged() {
    let database = database();
    database.execute("SET engine = 'compiled'").expect("the compiled engine");
    let sql = "SELECT sum(r) FROM (SELECT row_number() OVER (ORDER BY x) AS r FROM t) q";
    assert_eq!(rows(&database, sql).len(), 1);
    let log = database.refusals();
    assert_eq!(log.len(), 1, "{log:?}");
    assert!(log[0].ends_with(sql), "{log:?}");
}

#[test]
fn inner_joins_on_the_compiled_engine_answer_what_the_first_engine_answers() {
    let database = database();
    for sql in [
        "CREATE TABLE a (k INTEGER, k2 VARCHAR, v INTEGER)",
        "CREATE TABLE b (k INTEGER, k2 VARCHAR, w VARCHAR)",
        "INSERT INTO a SELECT CASE WHEN i % 11 = 0 THEN NULL ELSE i % 40 END, 'g' || (i % 3), i FROM range(500) r(i)",
        "INSERT INTO b SELECT CASE WHEN i % 13 = 0 THEN NULL ELSE i % 25 END, CASE WHEN i % 5 = 0 THEN NULL ELSE 'g' || (i % 3) END, 'a payload longer than twelve bytes ' || i FROM range(120) r(i)",
    ] {
        database.execute(sql).unwrap_or_else(|error| panic!("{sql} failed: {error}"));
    }
    let queries = [
        // Null keys on both sides, duplicate keys on both, and keys of a with no match in b.
        "SELECT a.k, a.v, b.w FROM a JOIN b ON a.k = b.k ORDER BY 1, 2, 3",
        "SELECT a.v, b.w FROM a JOIN b ON a.k = b.k AND a.k2 = b.k2 ORDER BY 1, 2",
        "SELECT a.v, b.w FROM a JOIN b ON a.k = b.k + 1000 ORDER BY 1, 2",
        "SELECT a.v, b.k FROM a JOIN b ON a.k = b.k AND a.v % 7 < b.k ORDER BY 1, 2",
        "SELECT b.k2, count(*), sum(a.v), max(b.w) FROM a JOIN b ON a.k = b.k GROUP BY b.k2 ORDER BY 1 NULLS FIRST",
        "SELECT a.v, b.w FROM a JOIN b ON a.k = b.k WHERE a.v > 100 ORDER BY a.v DESC, b.w LIMIT 10",
        "SELECT count(*), sum(c.k) FROM a JOIN b ON a.k = b.k JOIN b c ON b.k2 = c.k2",
        "SELECT count(*) FROM t x JOIN t y ON x.x = y.x WHERE x.x = 3",
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
    assert!(text.contains(" functions native, "), "{text}");
    assert!(text.contains(" generated in "), "{text}");
    assert!(text.contains(" compiled in "), "{text}");
    let text = explained(
        &database,
        "EXPLAIN (CODEGEN) SELECT count(*) FROM t a LEFT JOIN t b ON a.x = b.x AND b.s = 'w1'",
    );
    assert!(text.starts_with("refused: "), "{text}");
    assert_eq!(
        database.refusals(),
        Vec::<String>::new(),
        "an explain runs nothing, so it logs nothing"
    );
}

#[test]
fn a_top_n_over_wide_rows_reads_them_back_on_the_compiled_engine() {
    let database = Database::new();
    for sql in [
        "CREATE TABLE w (a INTEGER, b VARCHAR, c BIGINT, d DOUBLE, e VARCHAR, f INTEGER, g INTEGER, h VARCHAR, i BIGINT, j INTEGER)",
        "INSERT INTO w SELECT (i * 7919) % 2000, 'b' || i, i * 3, i / 4.0, CASE WHEN i % 3 = 0 THEN NULL ELSE 'e' END, i, -i, 'a longer string than twelve ' || i, i * i, i % 5 FROM range(2000) r(i)",
    ] {
        database.execute(sql).unwrap_or_else(|error| panic!("{sql} failed: {error}"));
    }
    let sql = "SELECT * FROM w ORDER BY a DESC LIMIT 7";
    database.execute("SET engine = 'compiled'").expect("the compiled engine");
    let text = explained(&database, &format!("EXPLAIN (CODEGEN) {sql}"));
    assert!(text.contains("fetch the rows"), "{text}");
    let compiled = rows(&database, sql);
    database.execute("SET engine = 'first'").expect("the first engine");
    assert_eq!(rows(&database, sql), compiled);
    assert_eq!(database.refusals(), Vec::<String>::new());
}

#[test]
fn every_tier_the_build_has_answers_what_the_first_engine_answers() {
    let database = database();
    let sql = "SELECT s, count(*), sum(x), min(x) FROM t WHERE x % 3 <> 1 GROUP BY s ORDER BY s NULLS FIRST";
    database.execute("SET engine = 'first'").expect("the first engine");
    let first = rows(&database, sql);
    database.execute("SET engine = 'compiled'").expect("the compiled engine");
    for tier in ["auto", "interp", "clif", "direct"] {
        let set = database.execute(&format!("SET qc_tier = '{tier}'"));
        if tier == "clif" && !cfg!(feature = "qc-clif") {
            let error = set.expect_err("clif needs the feature").to_string();
            assert!(error.contains("qc-clif"), "{error}");
            continue;
        }
        if tier == "direct" && !cfg!(target_arch = "x86_64") {
            let error = set.expect_err("direct needs x86-64").to_string();
            assert!(error.contains("x86-64"), "{error}");
            continue;
        }
        set.unwrap_or_else(|error| panic!("SET qc_tier = '{tier}' failed: {error}"));
        assert_eq!(database.setting("qc_tier").expect("qc_tier reads back"), tier);
        assert_eq!(rows(&database, sql), first, "{tier}");
    }
    let error = database.execute("SET qc_tier = 'llvm'").expect_err("no such tier").to_string();
    assert!(error.contains("auto, interp, clif, direct"), "{error}");
    assert_eq!(database.refusals(), Vec::<String>::new());
}

#[test]
fn leaving_a_technique_out_answers_the_same() {
    let database = Database::new();
    for sql in [
        "SET threads = 4",
        "CREATE TABLE u (k BIGINT, url VARCHAR)",
        "INSERT INTO u SELECT i % 70001, CASE i % 3 WHEN 0 THEN 'https://www.host' || (i % 11) || '.com/p' WHEN 1 THEN 'http://host' || (i % 13) || '.org/' ELSE 'ftp://x/' || i END FROM range(200000) r(i)",
    ] {
        database.execute(sql).unwrap_or_else(|error| panic!("{sql} failed: {error}"));
    }
    let queries = [
        "SELECT k, count(*) AS c FROM u GROUP BY k ORDER BY c DESC, k LIMIT 5",
        "SELECT count(*) FROM u WHERE url LIKE '%host1%'",
        "SELECT regexp_replace(url, '^https?://(?:www\\.)?([^/]+)/.*$', '\\1') AS h, count(*) AS c FROM u GROUP BY h ORDER BY c DESC, h LIMIT 5",
        "SELECT count(*), max(k) FROM u",
    ];
    let first: Vec<_> = queries.iter().map(|sql| rows(&database, sql)).collect();
    database.execute("SET engine = 'compiled'").expect("the compiled engine");
    for ablate in ["none", "probe", "like", "top", "lanes", "stats", "codes", "all"] {
        database.execute(&format!("SET qc_ablate = '{ablate}'")).expect("a switch");
        let back = database.setting("qc_ablate").expect("qc_ablate reads back");
        let all = "probe,like,top,lanes,stats,codes";
        assert_eq!(back, if ablate == "all" { all } else { ablate });
        for (sql, first) in queries.iter().zip(&first) {
            assert_eq!(&rows(&database, sql), first, "{sql} with {ablate} off");
        }
    }
    let error = database.execute("SET qc_ablate = 'probe,fast'").expect_err("no such switch");
    assert!(error.to_string().contains("probe, like"), "{error}");
    assert_eq!(database.refusals(), Vec::<String>::new());
}

#[test]
fn switching_tiers_at_every_morsel_answers_what_one_tier_answers() {
    let database = database();
    let sql = "SELECT s, count(*), sum(x), max(x) FROM t GROUP BY s ORDER BY s NULLS FIRST";
    database.execute("SET engine = 'compiled'").expect("the compiled engine");
    database.execute("SET qc_tier = 'interp'").expect("interp");
    let alone = rows(&database, sql);
    for switch in ["every:1", "random:3", "off"] {
        database.execute(&format!("SET qc_switch = '{switch}'")).expect("a switch");
        assert_eq!(database.setting("qc_switch").expect("qc_switch reads back"), switch);
        if cfg!(feature = "qc-clif") {
            database.execute("SET qc_tier = 'clif'").expect("clif");
        }
        assert_eq!(rows(&database, sql), alone, "{switch}");
    }
    for morsel in ["100", "4096", "0"] {
        database.execute(&format!("SET qc_morsel = {morsel}")).expect("a morsel size");
        assert_eq!(database.setting("qc_morsel").expect("qc_morsel reads back"), morsel);
        assert_eq!(rows(&database, sql), alone, "{morsel}");
    }
    let error = database.execute("SET qc_morsel = 'many'").expect_err("not a size");
    assert!(error.to_string().contains("number of rows"), "{error}");
    let error = database.execute("SET qc_switch = 'every:0'").expect_err("no such switch");
    assert!(error.to_string().contains("every:<n>"), "{error}");
    assert_eq!(database.refusals(), Vec::<String>::new());
}
