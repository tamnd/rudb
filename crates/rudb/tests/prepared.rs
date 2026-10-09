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
    let statement = db.prepare("SELECT $first, $name").expect("prepares");
    let result = statement
        .execute_named(&[("first", Value::Integer(1)), ("name", Value::Integer(2))])
        .expect("runs");
    assert_eq!(result.names(), ["$first", "$name"]);
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
fn a_parameter_in_a_plain_statement_says_it_has_no_value() {
    let db = Database::new();
    let refused = db.query("SELECT $2, $1").expect_err("there is no value for them");
    assert_eq!(refused.message(), "Values were not provided for the following parameters: 1, 2");
}

#[test]
fn a_named_parameter_in_a_plain_statement_reads_the_variable_of_its_name() {
    let db = Database::new();
    db.execute("SET VARIABLE animal = 'duck'").expect("sets it");
    let answer = db.query("SELECT $Animal").expect("reads the variable");
    assert_eq!(answer.value_at(0, 0), Value::Varchar("duck".into()));
    db.execute("SET VARIABLE \"1\" = 5").expect("sets it");
    let refused = db.query("SELECT $1").expect_err("a number never reads a variable");
    assert_eq!(refused.message(), "Values were not provided for the following parameters: 1");
}

#[test]
fn named_and_positional_parameters_do_not_mix() {
    let db = Database::new();
    let refused = db.execute("PREPARE q AS SELECT $1, $param").expect_err("both kinds");
    assert_eq!(refused.message(), "Mixing named and positional parameters is not supported yet");
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

/// The value `EXECUTE` answers with, after the statements before it have run.
fn executed(db: &Database, sql: &str) -> Value {
    db.execute(sql).unwrap_or_else(|error| panic!("{sql}: {error}")).value_at(0, 0)
}

/// The message `sql` fails with.
fn refused(db: &Database, sql: &str) -> String {
    db.execute(sql).map(|_| ()).expect_err(sql).to_string()
}

#[test]
fn prepare_and_execute_in_sql_take_values_by_position_or_by_name() {
    let db = Database::new();
    db.execute("PREPARE s AS SELECT $1 + 1").expect("prepares");
    assert_eq!(executed(&db, "EXECUTE s(41)"), Value::Integer(42));
    assert_eq!(executed(&db, "EXECUTE s(1 + 2)"), Value::Integer(4));
    db.execute("PREPARE s AS SELECT ?::INT * 2, ?").expect("prepares over the first");
    let pair = db.execute("EXECUTE s(21, 'x')").expect("runs");
    assert_eq!(pair.value_at(0, 0), Value::Integer(42));
    assert_eq!(pair.value_at(0, 1), Value::Varchar("x".into()));
    db.execute("PREPARE named AS SELECT $a || $b").expect("prepares");
    assert_eq!(executed(&db, "EXECUTE named(b := 'y', a := 'x')"), Value::Varchar("xy".into()));
    db.execute("PREPARE plain AS SELECT 42").expect("prepares");
    assert_eq!(executed(&db, "EXECUTE plain"), Value::Integer(42));
    assert_eq!(executed(&db, "EXECUTE plain()"), Value::Integer(42));
    // The name is found whatever case it was written in, quoted or not.
    db.execute("PREPARE \"Upper\" AS SELECT 1").expect("prepares");
    assert_eq!(executed(&db, "EXECUTE UPPER"), Value::Integer(1));
    db.execute("PREPARE nothing AS SELECT $1").expect("prepares");
    assert_eq!(executed(&db, "EXECUTE nothing(NULL)"), Value::Null);
}

#[test]
fn an_executed_insert_writes_each_time() {
    let db = Database::new();
    db.execute("CREATE TABLE t (a INTEGER)").expect("creates");
    db.execute("PREPARE s AS INSERT INTO t VALUES ($1)").expect("prepares");
    db.execute("EXECUTE s(1)").expect("inserts");
    db.execute("EXECUTE s(2)").expect("inserts");
    assert_eq!(executed(&db, "SELECT count(*) FROM t WHERE a < 3"), Value::BigInt(2));
}

#[test]
fn deallocate_forgets_the_statement() {
    let db = Database::new();
    db.execute("PREPARE s AS SELECT 42").expect("prepares");
    db.execute("DEALLOCATE s").expect("forgets");
    assert_eq!(refused(&db, "EXECUTE s"), "Binder Error: Prepared statement \"s\" does not exist");
    db.execute("PREPARE s AS SELECT 42").expect("prepares");
    db.execute("DEALLOCATE PREPARE s").expect("forgets");
    assert_eq!(
        refused(&db, "EXECUTE s(1)"),
        "Binder Error: Prepared statement \"s\" does not exist"
    );
    // A name that was never prepared is no error to forget.
    db.execute("DEALLOCATE nope").expect("says nothing");
}

#[test]
fn execute_refuses_what_the_pin_refuses_in_its_words() {
    let db = Database::new();
    db.execute("PREPARE one AS SELECT $1").expect("prepares");
    db.execute("PREPARE two AS SELECT ?, ?").expect("prepares");
    db.execute("PREPARE named AS SELECT $a").expect("prepares");
    db.execute("PREPARE typed AS SELECT $1::INT").expect("prepares");
    for (sql, message) in [
        (
            "EXECUTE one",
            "Invalid Input Error: Values were not provided for the following parameters: 1",
        ),
        (
            "EXECUTE two(1)",
            "Invalid Input Error: Values were not provided for the following parameters: 2",
        ),
        (
            "EXECUTE one(1, 2)",
            "Invalid Input Error: Parameter argument/count mismatch, identifiers of the excess \
             parameters: 2",
        ),
        (
            "EXECUTE named(1)",
            "Invalid Input Error: Parameter argument/count mismatch, identifiers of the excess \
             parameters: 1",
        ),
        (
            "EXECUTE one(a := 1)",
            "Invalid Input Error: Parameter argument/count mismatch, identifiers of the excess \
             parameters: a",
        ),
        (
            "EXECUTE one((SELECT 5))",
            "Invalid Input Error: Only scalar parameters, named parameters or NULL supported for \
             EXECUTE",
        ),
        (
            "EXECUTE one(a)",
            "Invalid Input Error: Only scalar parameters, named parameters or NULL supported for \
             EXECUTE",
        ),
        (
            "EXECUTE two(1, b := 2)",
            "Not implemented Error: Mixing named parameters and positional parameters is not \
             supported yet",
        ),
        ("EXECUTE nope(1)", "Binder Error: Prepared statement \"nope\" does not exist"),
        ("EXECUTE typed('abc')", "Conversion Error: Could not convert string 'abc' to INT32"),
    ] {
        assert_eq!(refused(&db, sql), message, "{sql}");
    }
}

#[test]
fn prepare_refuses_what_the_pin_refuses_when_it_prepares() {
    let db = Database::new();
    for (sql, start) in [
        (
            "PREPARE s AS SELECT * FROM nosuch",
            "Catalog Error: Table with name nosuch does not exist!",
        ),
        (
            "PREPARE s AS SELECT nosuch",
            "Binder Error: Referenced column \"nosuch\" was not found because the FROM clause is \
             missing",
        ),
        (
            "PREPARE s(INTEGER) AS SELECT $1",
            "Not implemented Error: TypeList for prepared statement has not been implemented.",
        ),
        (
            "PREPARE s AS PREPARE t AS SELECT 1",
            "Parser Error: PREPARE_STATEMENT is not a preparable statement",
        ),
        ("PREPARE s AS EXECUTE t", "Parser Error: EXECUTE_STATEMENT is not a preparable statement"),
    ] {
        let message = refused(&db, sql);
        assert!(message.starts_with(start), "{sql}: {message}");
    }
    // A table dropped after the statement was prepared is missing when it runs.
    db.execute("CREATE TABLE t (a INTEGER)").expect("creates");
    db.execute("PREPARE s AS SELECT * FROM t").expect("prepares");
    db.execute("DROP TABLE t").expect("drops");
    assert!(
        refused(&db, "EXECUTE s").starts_with("Catalog Error: Table with name t does not exist!")
    );
}

#[test]
fn a_name_with_no_from_clause_to_look_in_says_the_clause_is_missing() {
    let db = Database::new();
    for (sql, message) in [
        (
            "SELECT nosuch + 1",
            "Binder Error: Referenced column \"nosuch\" was not found because the FROM clause is \
             missing",
        ),
        ("SELECT a.b.c", "Binder Error: Referenced table \"a.b\" not found!"),
    ] {
        assert_eq!(refused(&db, sql), message, "{sql}");
    }
    // A subquery with no clause of its own still sees the columns around it.
    assert!(
        refused(&db, "SELECT 1 FROM range(0) t(x) WHERE (SELECT nosuch)")
            .starts_with("Binder Error: Referenced column \"nosuch\" not found in FROM clause!"),
    );
}

#[test]
fn duckdb_prepared_statements_lists_what_prepare_named_the_way_the_pin_does() {
    let db = Database::new();
    for sql in [
        "PREPARE p1 AS SELECT 42;",
        "CREATE TABLE tbl(a VARCHAR)",
        "PREPARE p2 AS INSERT INTO tbl VALUES ('test')",
        "PREPARE p3 AS SELECT 21, $1, $2",
        "PREPARE scan AS SELECT a FROM tbl",
        "PREPARE stamp AS SELECT now()",
    ] {
        db.execute(sql).unwrap_or_else(|error| panic!("{sql}: {error}"));
    }
    let listed = "SELECT string_agg(name || '|' || statement || '|' || \
                  coalesce(parameter_types::VARCHAR, 'NULL') || '|' || \
                  coalesce(result_types::VARCHAR, 'NULL'), ';' ORDER BY name) \
                  FROM duckdb_prepared_statements()";
    // A read of a table and a call settled per transaction both leave the types out, since the
    // pin plans those statements again at every `EXECUTE`.
    assert_eq!(
        executed(&db, listed),
        Value::Varchar(
            "p1|SELECT 42|NULL|[INTEGER];p2|INSERT INTO tbl (VALUES ('test'))|NULL|[BIGINT];\
             p3|SELECT 21, $1, $2|[UNKNOWN, UNKNOWN]|NULL;scan|SELECT a FROM tbl|NULL|NULL;\
             stamp|SELECT now()|NULL|NULL"
                .into()
        )
    );
    db.execute("DEALLOCATE p1").expect("deallocates");
    assert_eq!(executed(&db, "SELECT count(*) FROM pg_prepared_statements"), Value::BigInt(4));
}

#[test]
fn duckdb_dependencies_lists_indexes_foreign_keys_and_sequences_in_defaults() {
    let db = Database::new();
    for sql in [
        "CREATE TABLE p(k INTEGER PRIMARY KEY)",
        "CREATE TABLE c(k INTEGER REFERENCES p(k))",
        "CREATE INDEX ci ON c(k)",
        "CREATE SEQUENCE s",
        "CREATE TABLE d(x INTEGER DEFAULT nextval('s'))",
        "CREATE VIEW v AS SELECT * FROM d",
        "CREATE TABLE o(x INTEGER)",
        "CREATE SEQUENCE sq2",
        "ALTER SEQUENCE sq2 OWNED BY o",
    ] {
        db.execute(sql).unwrap_or_else(|error| panic!("{sql}: {error}"));
    }
    let named = "WITH names AS (SELECT table_oid AS oid, table_name AS n FROM duckdb_tables() \
                 UNION ALL SELECT index_oid, index_name FROM duckdb_indexes() \
                 UNION ALL SELECT sequence_oid, sequence_name FROM duckdb_sequences()) \
                 SELECT string_agg(o.n || '>' || r.n || ':' || d.deptype, ',' ORDER BY o.n, r.n) \
                 FROM duckdb_dependencies() d JOIN names o ON o.oid = d.objid \
                 JOIN names r ON r.oid = d.refobjid";
    // The view is not there, which is the pin's: it lists no row for a view.
    assert_eq!(executed(&db, named), Value::Varchar("c>ci:a,p>c:n,s>d:n,sq2>o:a".into()));
    let zeros =
        "SELECT count(*) FROM pg_depend WHERE classid + objsubid + refclassid + refobjsubid = 0";
    assert_eq!(executed(&db, zeros), Value::BigInt(4));
}
