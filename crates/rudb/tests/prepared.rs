//! Prepared statements: parameters, the values they are given, and what is said when they are not.
//!
//! Every sentence asserted here was read off duckdb v1.4.1 first, using `PREPARE` and `EXECUTE`,
//! which is the same machinery reached from SQL instead of from a program.

use rudb::Database;
use rudb_common::Value;

/// A database with a small table in it.
fn seeded() -> Database {
    let db = Database::new();
    db.execute("CREATE TABLE t (a INTEGER, b VARCHAR)").expect("creates");
    db.execute("INSERT INTO t VALUES (1, 'one'), (2, 'two'), (3, 'three')").expect("inserts");
    db
}

#[test]
fn a_parameter_is_the_value_it_was_given() {
    let db = seeded();
    let counted = db.prepare("SELECT count(*) FROM t WHERE a > ?").expect("prepares");
    assert_eq!(counted.value(&[Value::Integer(1)]).expect("runs"), Value::BigInt(2));
    // The point of preparing it is the second run, which parses nothing.
    assert_eq!(counted.value(&[Value::Integer(2)]).expect("runs"), Value::BigInt(1));
    assert_eq!(counted.value(&[Value::Integer(9)]).expect("runs"), Value::BigInt(0));
}

#[test]
fn the_four_ways_to_write_a_parameter_all_work() {
    let db = Database::new();
    for sql in ["SELECT ?", "SELECT ?1", "SELECT $1"] {
        let statement = db.prepare(sql).expect("prepares");
        assert_eq!(statement.parameters(), ["1"], "{sql}");
        assert_eq!(
            statement.value(&[Value::Integer(7)]).expect("runs"),
            Value::Integer(7),
            "{sql}"
        );
    }
    // A named one is given its value by name, which is the same rule duckdb has: `EXECUTE p(1)` on
    // a statement written with `$x` provides nothing for `x`.
    let named = db.prepare("SELECT $x").expect("prepares");
    assert_eq!(named.parameters(), ["x"]);
    let result = named.execute_named(&[("x", Value::Integer(7))]).expect("runs");
    assert_eq!(result.value_at(0, 0), Value::Integer(7));
}

#[test]
fn a_bare_question_mark_is_numbered_by_where_it_was_written() {
    let db = Database::new();
    let pair = db.prepare("SELECT ?, ?").expect("prepares");
    assert_eq!(pair.parameters(), ["1", "2"]);
    let result = pair.execute(&[Value::Integer(10), Value::Integer(20)]).expect("runs");
    assert_eq!(result.value_at(0, 0), Value::Integer(10));
    assert_eq!(result.value_at(0, 1), Value::Integer(20));
}

#[test]
fn a_parameter_used_twice_is_one_value() {
    let db = Database::new();
    let twice = db.prepare("SELECT $1 + $1").expect("prepares");
    assert_eq!(twice.parameters(), ["1"]);
    assert_eq!(twice.value(&[Value::Integer(21)]).expect("runs"), Value::Integer(42));
}

#[test]
fn a_named_parameter_is_matched_without_regard_to_case() {
    // The one place the dialect folds case. `PREPARE p AS SELECT $A` runs with `EXECUTE p(a := 1)`.
    let db = Database::new();
    let named = db.prepare("SELECT $A").expect("prepares");
    let result = named.execute_named(&[("a", Value::Integer(1))]).expect("runs");
    assert_eq!(result.value_at(0, 0), Value::Integer(1));
}

#[test]
fn the_column_is_named_after_the_parameter_rather_than_after_the_value() {
    let db = Database::new();
    let statement = db.prepare("SELECT $1, $name").expect("prepares");
    let result = statement
        .execute_named(&[("1", Value::Integer(1)), ("name", Value::Integer(2))])
        .expect("runs");
    assert_eq!(result.names(), ["$1", "$name"]);
}

#[test]
fn a_parameter_with_no_value_says_which_one() {
    let db = Database::new();
    let statement = db.prepare("SELECT $a, $b").expect("prepares");
    // Positional values on a statement written with names provide nothing it asked for, which is
    // the sentence duckdb gives for `EXECUTE p(1, 2)` there.
    let refused = statement
        .execute(&[Value::Integer(1), Value::Integer(2)])
        .expect_err("nothing it wanted was provided");
    assert_eq!(
        refused.message(),
        "Values were not provided for the following prepared statement parameters: a, b"
    );
}

#[test]
fn a_value_for_a_parameter_that_is_not_there_says_which_one() {
    let db = Database::new();
    let statement = db.prepare("SELECT $2").expect("prepares");
    let refused = statement
        .execute(&[Value::Integer(1), Value::Integer(2)])
        .expect_err("the first value has nowhere to go");
    assert_eq!(
        refused.message(),
        "Parameter argument/count mismatch, identifiers of the excess parameters: 1"
    );
}

#[test]
fn a_parameter_in_a_plain_statement_says_to_prepare_it_first() {
    let db = Database::new();
    let refused = db.query("SELECT $1").expect_err("there is no value for it");
    assert!(
        refused.message().starts_with("Prepared statement parameters cannot be used directly"),
        "{}",
        refused.message()
    );
}

#[test]
fn a_statement_that_writes_takes_parameters_too() {
    let db = Database::new();
    db.execute("CREATE TABLE t (a INTEGER, b VARCHAR)").expect("creates");
    let insert = db.prepare("INSERT INTO t VALUES (?, ?)").expect("prepares");
    insert.execute(&[Value::Integer(1), Value::Varchar("one".into())]).expect("inserts");
    insert.execute(&[Value::Integer(2), Value::Varchar("two".into())]).expect("inserts");
    assert_eq!(db.value("SELECT count(*) FROM t").expect("counts"), Value::BigInt(2));
    assert_eq!(
        db.value("SELECT b FROM t WHERE a = 2").expect("reads"),
        Value::Varchar("two".into())
    );
}

#[test]
fn every_type_the_engine_has_can_be_a_parameter() {
    let db = Database::new();
    let statement = db.prepare("SELECT ?").expect("prepares");
    let values = vec![
        Value::Boolean(true),
        Value::TinyInt(-1),
        Value::SmallInt(-2),
        Value::Integer(-3),
        Value::BigInt(-4),
        Value::HugeInt(-5),
        Value::UTinyInt(1),
        Value::USmallInt(2),
        Value::UInteger(3),
        Value::UBigInt(4),
        Value::Float(1.5),
        Value::Double(2.5),
        Value::Varchar("text".into()),
        Value::Blob(vec![0, 1, 2]),
        Value::Date(19723),
        Value::Time(3_600_000_000),
        Value::Timestamp(1_700_000_000_000_000),
        Value::Interval { months: 1, days: 2, micros: 3 },
        Value::Decimal { width: 9, scale: 2, unscaled: 12345 },
        Value::Null,
    ];
    for value in values {
        let back = statement.value(std::slice::from_ref(&value)).expect("runs");
        assert_eq!(back, value);
    }
}

#[test]
fn a_prepared_statement_belongs_to_the_database_it_came_from() {
    let db = seeded();
    let connection = db.connect();
    let statement = connection.prepare("SELECT sum(a) FROM t WHERE a >= ?").expect("prepares");
    assert_eq!(statement.value(&[Value::Integer(2)]).expect("runs"), Value::HugeInt(5));
    // A write through the database is visible to a statement prepared before it happened, because
    // what is held is the statement and not the data it reads.
    db.execute("INSERT INTO t VALUES (10, 'ten')").expect("inserts");
    assert_eq!(statement.value(&[Value::Integer(2)]).expect("runs"), Value::HugeInt(15));
    assert_eq!(statement.sql(), "SELECT sum(a) FROM t WHERE a >= ?");
}

#[test]
fn a_statement_that_does_not_parse_is_an_error_at_prepare_time() {
    let db = Database::new();
    assert!(db.prepare("SELECT FROM WHERE").is_err());
    // A name that does not resolve is not, because a parameter has no type until it has a value and
    // so binding cannot happen until then.
    let statement = db.prepare("SELECT * FROM nosuchtable WHERE a = ?").expect("parses");
    assert!(statement.execute(&[Value::Integer(1)]).is_err());
}
