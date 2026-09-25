//! A query's time split by kind of work: scan, filter, hash build, probe, aggregate, strings and
//! materialization.
//!
//! Milestone C0 asks for the counters that say which of those a query spends itself on. These tests
//! run a query that does all of them and check that each part was charged, that the parts come out
//! of the operators' own time rather than on top of it, and that the split is readable from the
//! document, from `EXPLAIN ANALYZE` and from `rudb_statement_metrics()`.
//!
//! No test asserts a duration, for the reason the phase tests give: a threshold in nanoseconds is a
//! threshold about the machine.

use rudb::Database;
use rudb_common::Value;

/// A fact table with a string column and a small dimension to join it to.
fn tables(connection: &rudb::Connection) {
    connection
        .execute(
            "CREATE TABLE facts AS SELECT i AS k, i % 100 AS d, 'name ' || CAST(i AS VARCHAR) AS s \
             FROM range(0, 20000) AS r(i)",
        )
        .expect("builds the facts");
    connection
        .execute(
            "CREATE TABLE dims AS SELECT i AS d, 'dim ' || CAST(i AS VARCHAR) AS label \
             FROM range(0, 100) AS r(i)",
        )
        .expect("builds the dimension");
}

const QUERY: &str = "SELECT dims.label, count(*), max(facts.s) FROM facts JOIN dims \
                     ON facts.d = dims.d WHERE facts.s LIKE '%7%' GROUP BY dims.label";

#[test]
fn a_join_with_a_string_filter_charges_every_part() {
    let database = Database::new();
    let connection = database.connect();
    tables(&connection);
    let result = connection.query(QUERY).expect("runs the query");
    let metrics = result.metrics().expect("a query that ran has metrics");
    let split = metrics.split();
    assert!(split.scan_ns > 0, "{split:?}");
    assert!(split.build_ns > 0, "{split:?}");
    assert!(split.probe_ns > 0, "{split:?}");
    assert!(split.aggregate_ns > 0, "{split:?}");
    assert!(split.strings_ns > 0, "the LIKE was charged nothing: {split:?}");
    assert!(split.materialize_ns > 0, "the matched columns were charged nothing: {split:?}");
    let own: u64 = metrics.operators.iter().map(|operator| operator.wall_ns).sum();
    assert!(
        split.total() >= own + metrics.timing.result_ns,
        "every operator's time is somewhere in the split: {split:?} against {own}"
    );
    let json = metrics.render();
    assert!(json.contains("\"split\""), "{json}");
    assert!(json.contains("\"strings_ns\""), "{json}");
}

#[test]
fn explain_analyze_prints_the_split() {
    let database = Database::new();
    let connection = database.connect();
    tables(&connection);
    let result = connection.query(&format!("EXPLAIN ANALYZE {QUERY}")).expect("explains");
    let Value::Varchar(text) = result.value_at(0, 1) else { panic!("the plan is text") };
    assert!(text.contains("by kind of work: "), "{text}");
    assert!(text.contains(" probe"), "{text}");
}

#[test]
fn the_statement_ring_keeps_the_split() {
    let database = Database::new();
    let connection = database.connect();
    tables(&connection);
    let sql = format!("{QUERY} ORDER BY 1");
    connection.query(&sql).expect("runs the query");
    let result = connection
        .query("SELECT * FROM rudb_statement_metrics() ORDER BY id DESC")
        .expect("reads the ring");
    let names = result.names().to_vec();
    let row = result
        .rows()
        .find(|row| row[1] == Value::Varchar(sql.clone()))
        .expect("the query was kept");
    let part = |name: &str| {
        let at = names.iter().position(|named| named == name).expect(name);
        match &row[at] {
            Value::BigInt(nanos) => *nanos,
            other => panic!("{name} is {other:?}"),
        }
    };
    for name in ["scan_ns", "build_ns", "probe_ns", "aggregate_ns", "strings_ns"] {
        assert!(part(name) > 0, "{name} was charged nothing");
    }
}
