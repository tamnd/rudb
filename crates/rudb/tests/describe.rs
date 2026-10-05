//! `Prepared::describe`: the types of the parameters and the columns of the answer, found without
//! running the statement.

use rudb::Database;
use rudb_common::{Field, LogicalType};

fn database() -> Database {
    let db = Database::new();
    db.execute("CREATE TABLE t (id BIGINT, name VARCHAR, price DOUBLE, born DATE)").unwrap();
    db
}

fn types(db: &Database, sql: &str, declared: &[Option<LogicalType>]) -> Vec<Option<LogicalType>> {
    db.prepare(sql).unwrap().describe(declared).unwrap().parameters
}

#[test]
fn a_parameter_takes_the_type_of_what_it_meets() {
    let db = database();
    let sql = "SELECT * FROM t WHERE id = $1 AND name = $2 AND born > $3";
    assert_eq!(
        types(&db, sql, &[]),
        [Some(LogicalType::BigInt), Some(LogicalType::Varchar), Some(LogicalType::Date)]
    );
    let sql = "INSERT INTO t VALUES ($1, $2, $3, $4)";
    assert_eq!(
        types(&db, sql, &[]),
        [
            Some(LogicalType::BigInt),
            Some(LogicalType::Varchar),
            Some(LogicalType::Double),
            Some(LogicalType::Date)
        ]
    );
    let sql = "UPDATE t SET price = $2 WHERE id = $1";
    assert_eq!(types(&db, sql, &[]), [Some(LogicalType::Double), Some(LogicalType::BigInt)]);
    assert_eq!(types(&db, "DELETE FROM t WHERE born < $1", &[]), [Some(LogicalType::Date)]);
    assert_eq!(types(&db, "SELECT $1", &[]), [None]);
    let sql = "INSERT INTO t (id, name) VALUES ($1, $2), ($3, 'b')";
    assert_eq!(
        types(&db, sql, &[]),
        [Some(LogicalType::BigInt), Some(LogicalType::Varchar), Some(LogicalType::BigInt)]
    );
    let sql = "INSERT INTO t (price) SELECT $1";
    assert_eq!(types(&db, sql, &[]), [Some(LogicalType::Double)]);
    let sql = "SELECT count(*) FROM t WHERE id IN ($1, $2)";
    assert_eq!(types(&db, sql, &[]), [Some(LogicalType::BigInt), Some(LogicalType::BigInt)]);
}

#[test]
fn a_declared_type_is_kept() {
    let db = database();
    let declared = [Some(LogicalType::Integer), None];
    let sql = "SELECT $1, name FROM t WHERE id = $2";
    let description = db.prepare(sql).unwrap().describe(&declared).unwrap();
    assert_eq!(description.parameters, [Some(LogicalType::Integer), Some(LogicalType::BigInt)]);
    let fields = description.fields.unwrap();
    assert_eq!(fields[0].ty, LogicalType::Integer);
    assert_eq!(fields[1], Field::new("name", LogicalType::Varchar));
}

#[test]
fn the_columns_of_the_answer() {
    let db = database();
    let fields = |sql: &str| db.prepare(sql).unwrap().describe(&[]).unwrap().fields;
    assert_eq!(
        fields("SELECT id, price * 2 AS twice FROM t WHERE id = $1"),
        Some(vec![Field::new("id", LogicalType::BigInt), Field::new("twice", LogicalType::Double)])
    );
    assert_eq!(
        fields("INSERT INTO t (id) VALUES ($1) RETURNING id, name"),
        Some(vec![Field::new("id", LogicalType::BigInt), Field::new("name", LogicalType::Varchar)])
    );
    assert_eq!(fields("INSERT INTO t (id) VALUES ($1)"), None);
    assert_eq!(fields("CREATE TABLE u (a INTEGER)"), None);
    assert_eq!(db.query("SELECT count(*) FROM t").unwrap().len(), 1);
}

#[test]
fn a_missing_table_is_an_error_and_nothing_runs() {
    let db = database();
    let error = db.prepare("SELECT * FROM nope WHERE a = $1").unwrap().describe(&[]).unwrap_err();
    assert!(error.message().contains("nope"), "{}", error.message());
    db.prepare("INSERT INTO t (id) VALUES ($1)").unwrap().describe(&[]).unwrap();
    let count = db.prepare("SELECT count(*) FROM t").unwrap().value(&[]).unwrap();
    assert_eq!(count, rudb_common::Value::BigInt(0));
}
