//! Reading back the settings rudb has that DuckDB does not.
//!
//! `SET` takes four names that are not in the DuckDB settings catalog and all four take effect.
//! Until #1295 none of them could be read from SQL, because `current_setting` resolves against that
//! catalog and a name it does not hold is a name nobody has. The only reader was `Database::setting`,
//! which is a Rust call, so a session driving the engine through SQL could write one of these and
//! then had no way to ask what it said.
//!
//! That is worst for `cluster_by`, because the declaration is not session state. It lives on the
//! tables, so a database opened on a file carries declarations that nobody in this session wrote and
//! reading it back is the only way to find out what they are.
//!
//! Three of the four are covered here. The seam settings are the fourth and they still answer with
//! the catalog error, because the seam names live in a crate the binder does not depend on.

use rudb::Database;
use rudb_common::Value;

/// The two tables the declarations below name, with the columns they name on them.
fn database() -> Database {
    let database = Database::new();
    database.execute("CREATE TABLE orders (o_orderkey BIGINT, o_orderdate DATE)").expect("creates");
    database
        .execute("CREATE TABLE lineitem (l_orderkey BIGINT, l_linenumber INTEGER, l_shipdate DATE)")
        .expect("creates");
    database
}

/// The one value a query answers with.
fn value(database: &Database, sql: &str) -> Value {
    database.value(sql).unwrap_or_else(|error| panic!("{sql} failed: {error}"))
}

/// The text of a one value query, which is what `SHOW` and a VARCHAR setting both answer with.
fn text(database: &Database, sql: &str) -> String {
    match value(database, sql) {
        Value::Varchar(text) => text,
        other => panic!("{sql} answered with {other}, which is not text"),
    }
}

#[test]
fn a_row_order_declaration_reads_back_as_the_text_it_was_written_as() {
    let database = database();
    let declaration = "lineitem(quarter(l_shipdate), l_orderkey, l_linenumber)";
    database
        .execute(&format!("SET cluster_by = '{declaration}'"))
        .expect("declares the order the rows are stored in");

    assert_eq!(text(&database, "SELECT current_setting('cluster_by')"), declaration);
    assert_eq!(text(&database, "SHOW cluster_by"), declaration);
    assert_eq!(
        database.setting("cluster_by").expect("the Rust reader"),
        declaration,
        "both readers answer the same text"
    );
}

#[test]
fn two_declarations_read_back_in_the_spelling_the_setting_takes() {
    let database = database();
    database
        .execute("SET cluster_by = 'lineitem(quarter(l_shipdate), l_orderkey), orders(o_orderkey)'")
        .expect("declares both");

    // Catalog order and not the order they were written in, because the answer is built out of the
    // tables rather than out of a copy of the text, which is the whole reason it is right about a
    // file somebody else wrote.
    let read = text(&database, "SELECT current_setting('cluster_by')");
    assert_eq!(read, "orders(o_orderkey), lineitem(quarter(l_shipdate), l_orderkey)");

    database.execute(&format!("SET cluster_by = '{read}'")).expect("takes its own answer back");
    assert_eq!(text(&database, "SELECT current_setting('cluster_by')"), read, "it round trips");
}

#[test]
fn a_database_that_has_declared_nothing_reads_back_as_nothing() {
    let database = database();
    assert_eq!(text(&database, "SELECT current_setting('cluster_by')"), "");
    assert_eq!(text(&database, "SELECT current_setting('graph_links')"), "");
}

#[test]
fn the_relationships_read_back_as_they_were_written() {
    let database = database();
    let links = "lineitem(l_orderkey) -> orders(o_orderkey)";
    database.execute(&format!("SET graph_links = '{links}'")).expect("declares the relationship");

    assert_eq!(text(&database, "SELECT current_setting('graph_links')"), links);
    assert_eq!(text(&database, "SHOW graph_links"), links);
}

#[test]
fn a_rule_switch_reads_back_as_a_boolean() {
    let database = database();
    assert_eq!(value(&database, "SELECT current_setting('stats.presize')"), Value::Boolean(true));
    assert_eq!(text(&database, "SELECT typeof(current_setting('stats.presize'))"), "BOOLEAN");

    database.execute("SET stats_presize = false").expect("turns the rule off");
    assert_eq!(value(&database, "SELECT current_setting('stats.presize')"), Value::Boolean(false));
    assert_eq!(text(&database, "SHOW stats_presize"), "false");
}

#[test]
fn a_rule_that_starts_off_reads_back_as_off() {
    let database = database();
    assert_eq!(value(&database, "SELECT current_setting('graph.sections')"), Value::Boolean(false));
}

#[test]
fn a_mistyped_rule_is_told_what_the_rules_are_by_both_readers() {
    let database = database();
    let refused =
        database.query("SELECT current_setting('stats.presise')").expect_err("no such rule");
    assert!(refused.to_string().contains("stats.presize"), "{refused}");

    let refused = database.query("SHOW stats_presise").expect_err("no such rule");
    assert!(refused.to_string().contains("stats.presize"), "{refused}");
}

#[test]
fn a_name_nobody_has_is_still_refused_in_the_words_each_statement_uses() {
    let database = database();
    let refused =
        database.query("SELECT current_setting('no_such_setting')").expect_err("no such setting");
    assert!(refused.to_string().contains("no_such_setting"), "{refused}");

    let refused = database.query("SHOW no_such_setting").expect_err("no such setting");
    assert!(
        refused.to_string().contains("Setting with name \"no_such_setting\" does not exist"),
        "{refused}"
    );
}

#[test]
fn reading_one_back_does_not_put_it_in_the_list_of_duckdb_settings() {
    let database = database();
    database
        .execute("SET cluster_by = 'orders(o_orderkey)'")
        .expect("declares the order the rows are stored in");

    let listed = database
        .query("SELECT name FROM duckdb_settings() WHERE name IN ('cluster_by', 'graph_links')")
        .expect("lists the settings");
    assert_eq!(listed.len(), 0, "neither of them is a DuckDB setting and neither is listed");
}
