//! The ablation that has to pass on every commit, plus the settings surface it runs through.
//!
//! `spec/stats/09-measurement.md` section 9.3 and `spec/graph/09-measurement.md` section 9.2 both
//! ask for the same thing: run the suite with the layer off and with the layer on, and compare the
//! answers. Not similar, identical. It is the correctness claim the whole statistics and graph
//! effort rests on, and it is worth landing before there is anything to ablate, because the day it
//! starts failing is the day somebody made a statistic change an answer, and the difference between
//! catching that at the commit and catching it at the next SF100 run is the difference between
//! bisecting one commit and bisecting a week.
//!
//! Today every switch is trivially satisfied: nothing consumes a statistic and there are no graph
//! sections, so all three runs take the same code path. That is the point of landing it at G0.

use rudb::Database;
use rudb_common::Value;

/// A database with enough shape in it that the queries below exercise more than one operator.
fn database() -> Database {
    let database = Database::new();
    database.execute("CREATE TABLE t (a INTEGER, b BIGINT, c VARCHAR)").expect("creates");
    database
        .execute(
            "INSERT INTO t SELECT r::INTEGER, (r * 7)::BIGINT, 'row ' || r::VARCHAR \
             FROM range(2000) AS s(r)",
        )
        .expect("inserts");
    database
}

/// Every row of a query, as values.
fn rows(database: &Database, sql: &str) -> Vec<Vec<Value>> {
    let result = database.query(sql).unwrap_or_else(|error| panic!("{sql} failed: {error}"));
    (0..result.len())
        .map(|row| (0..result.width()).map(|column| result.value_at(row, column)).collect())
        .collect()
}

/// The shapes every rule in `spec/stats/05-every-query.md` is aimed at, which is what the ablation
/// has to cover for the comparison to mean anything.
const QUERIES: &[&str] = &[
    "SELECT count(*) FROM t",
    "SELECT a, b, c FROM t WHERE a % 97 = 0 ORDER BY a",
    "SELECT count(*), sum(b), min(c), max(c) FROM t WHERE a > 900",
    "SELECT b % 5 AS k, count(*) FROM t GROUP BY k ORDER BY k",
    "SELECT a FROM t WHERE c LIKE 'row 1%' AND b > 100 ORDER BY a LIMIT 25",
    "SELECT DISTINCT a % 11 AS k FROM t ORDER BY k",
    "SELECT l.a, r.c FROM t AS l JOIN t AS r ON l.a = r.a WHERE l.a % 251 = 0 ORDER BY l.a",
    "SELECT a, sum(b) OVER (ORDER BY a ROWS 3 PRECEDING) FROM t WHERE a < 40 ORDER BY a",
    "SELECT c FROM t WHERE a IS NULL",
];

#[test]
fn the_ablation_changes_no_answer() {
    let database = database();
    for sql in QUERIES {
        let expected = rows(&database, sql);
        for switch in ["SET statistics = 'off'", "SET graph_sections = 'off'"] {
            database.execute(switch).expect("the switch is a setting");
            assert_eq!(rows(&database, sql), expected, "{switch} changed {sql}");
        }
        database.execute("RESET statistics").expect("and goes back");
        database.execute("RESET graph_sections").expect("and goes back");
        assert_eq!(rows(&database, sql), expected, "resetting the switches changed {sql}");
    }
}

#[test]
fn every_rule_goes_off_on_its_own_without_changing_an_answer() {
    let database = database();
    let rules = [
        "stats_presize",
        "stats_direct_addressing",
        "stats_validity_free",
        "stats_filter_order",
        "stats_top_n_seed",
        "stats_narrow_arithmetic",
        "stats_join_elimination",
        "stats_memory_reservation",
    ];
    for sql in QUERIES {
        let expected = rows(&database, sql);
        for rule in rules {
            database.execute(&format!("SET {rule} = false")).expect("one rule off");
            assert_eq!(rows(&database, sql), expected, "{rule} changed {sql}");
            database.execute(&format!("RESET {rule}")).expect("and back on");
        }
    }
}

#[test]
fn a_rule_reads_back_the_way_it_was_set() {
    let database = database();
    let setting = |name: &str| database.setting(name).unwrap_or_else(|error| panic!("{error}"));

    assert_eq!(setting("stats.presize"), "true");
    database.execute("SET stats_presize = false").expect("sets");
    assert_eq!(setting("stats.presize"), "false");
    assert_eq!(setting("stats_presize"), "false");
    database.execute("RESET stats_presize").expect("resets");
    assert_eq!(setting("stats.presize"), "true");
}

#[test]
fn the_two_masters_answer_to_the_names_the_specification_uses() {
    let database = database();
    let setting = |name: &str| database.setting(name).unwrap_or_else(|error| panic!("{error}"));

    database.execute("SET statistics = 'off'").expect("the specification spelling");
    assert_eq!(setting("statistics"), "false");
    assert_eq!(setting("stats.all"), "false");
    // A rule's own switch is where the session left it, which is what it reads back as. Whether it
    // may fire is the master and the switch together, and that is not a question a setting answers.
    assert_eq!(setting("stats.presize"), "true");
    database.execute("SET statistics = 'on'").expect("and back");
    assert_eq!(setting("stats.all"), "true");

    database.execute("SET graph_sections = 'off'").expect("the graph spelling");
    assert_eq!(setting("graph.sections"), "false");
    database.execute("RESET graph_sections").expect("and back");
    assert_eq!(setting("graph.sections"), "true");
}

#[test]
fn a_mistyped_rule_is_told_what_the_rules_are() {
    let database = database();
    let refused = database.execute("SET stats_presise = false").expect_err("no such rule");
    let message = refused.to_string();
    assert!(message.contains("stats.presize"), "{message}");

    let refused = database.setting("stats.presise").expect_err("no such rule");
    assert!(refused.to_string().contains("stats.presize"), "{refused}");
}

#[test]
fn the_rules_are_not_duckdb_settings() {
    let database = database();
    let listed = rows(&database, "SELECT name FROM duckdb_settings() ORDER BY name");
    for row in &listed {
        let Value::Varchar(name) = &row[0] else { panic!("a setting name is a string") };
        assert!(!name.starts_with("stats."), "{name} is listed and DuckDB has never heard of it");
        assert!(!name.starts_with("graph."), "{name} is listed and DuckDB has never heard of it");
        assert_ne!(name, "statistics");
        assert_ne!(name, "graph_sections");
    }
}
